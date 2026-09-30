//! AMD's own decoder, through its runtime, on a device of the library's own:
//! the runtime reads and decodes each unit, and the readers here say which
//! picture leaves when.
//!
//! **The readers stay in front.** The runtime parses the stream itself, so a
//! unit goes to it whole; the readers read it too, for what the runtime does
//! not say before it decodes -- the stream's size, depth and range, a change
//! of format, a unit that cannot be read -- and for the order pictures leave
//! in. The runtime is asked to hand every picture out as soon as its unit is
//! decoded, in decode order, and to decode in its low-latency mode, whose
//! clock does not fall between pictures paced in real time; the readers'
//! picture buffer then lets each out in the stream's own order, a picture's
//! surface held until its turn -- none held at all on a stream that does not
//! reorder, which is every host's.
//!
//! **Nothing waits for a decode.** A picture is handed out as soon as its unit
//! is submitted, its decode still running; the split and a read-back's copy
//! are queued on the same device behind it, which orders them, and the
//! library's fence is signalled behind the split. The runtime would queue
//! some thirty units before pushing back, so no more than two split pictures
//! are left unfinished when the next unit goes in: past that the decode
//! thread sleeps on the fence, which only a device fallen behind the stream
//! makes it do.
//!
//! **One buffer carries every unit**, made with the backend: the runtime
//! keeps no reference to a unit once it is submitted.

use core::fmt;
use core::time::Duration;
use std::sync::Arc;

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::amf::{
    self as runtime, Amf, Buffer, Component, Context, LOW_LATENCY_DECODE, Layout,
    REORDER_LOW_LATENCY, REORDER_MODE, Submitted, Surface,
};
use lowlat_drivers::d3d11::{Com, Device, Error as DeviceError, Event, Fence, SharedTexture};
use lowlat_drivers::ffi::d3d11::{
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, DXGI_FORMAT, ID3D11Resource,
    ID3D11ShaderResourceView, ID3D11Texture2D,
};
use lowlat_drivers::vcall;

use crate::d3d11;
use crate::split::{self, Split, copy_planes, micros};
use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes, h264, hevc};

/// Surfaces the readers' slots name: what either picture buffer indexes.
const SURFACES: usize = h264::dpb::MAX_FRAMES;
const _: () = assert!(hevc::dpb::MAX_PICTURES <= SURFACES);
/// Units submitted whose picture has not come out yet, found by the mark
/// each carries.
const PENDING: usize = 8;
/// Views kept, a pair per texture the decoder hands out: it hands the same
/// six to eight out over and over.
const VIEWS: usize = 16;
/// Pictures split and not yet finished on the device when the next unit is
/// submitted, at most.
pub const MOST_UNFINISHED: u64 = 2;
/// How long a device fallen behind is slept on before the decoder is given
/// up.
const SETTLE_WAIT: Duration = Duration::from_secs(1);
/// Times a unit holding more than one picture is submitted again.
const REPEAT_TRIES: u32 = 8;
/// The size a decoder is built at to ask whether it builds at all.
const PROBE_SIZE: (u32, u32) = (1280, 720);

/// Why a decoder could not be built or a picture could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// AMD's runtime refused a call.
    Runtime(runtime::Error),
    /// The device: the split, a read-back, or the device lost.
    Device(d3d11::Error),
    /// A stream no decoder here takes -- full or half chroma, the first
    /// codec above eight bits -- or a decoder the runtime would not build.
    NoProfile,
    /// A unit the readers could not read, or a picture the runtime never
    /// handed out.
    Stream,
    /// A picture larger than the caller's planes, or a unit larger than the
    /// buffer.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "{e}"),
            Self::Device(e) => write!(f, "{e}"),
            Self::NoProfile => f.write_str("AMD decoder takes no stream of this shape"),
            Self::Stream => f.write_str("AMD decoder lost a picture of the stream"),
            Self::TooLarge => f.write_str("picture or unit larger than the decoder holds"),
        }
    }
}

impl std::error::Error for Error {}

impl From<runtime::Error> for Error {
    fn from(e: runtime::Error) -> Self {
        match e {
            runtime::Error::TooLarge => Self::TooLarge,
            other => Self::Runtime(other),
        }
    }
}

impl From<d3d11::Error> for Error {
    fn from(e: d3d11::Error) -> Self {
        Self::Device(e)
    }
}

impl From<DeviceError> for Error {
    fn from(e: DeviceError) -> Self {
        Self::Device(d3d11::Error::from(e))
    }
}

type Result<T> = core::result::Result<T, Error>;

// SAFETY: every structure this is used for is plain data the device's
// headers define, whose all-zero value is valid and is where a fill starts.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

/// What a decoder is built for: the codec, and the depth, which the layout
/// pictures are handed out in follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    codec: Codec,
    ten_bit: bool,
}

impl Shape {
    const fn decoder(self) -> runtime::Codec {
        match self.codec {
            Codec::H264 => runtime::Codec::H264,
            Codec::H265 => runtime::Codec::Hevc,
        }
    }

    const fn layout(self) -> Layout {
        if self.ten_bit {
            Layout::P010
        } else {
            Layout::Nv12
        }
    }

    const fn format(self) -> Format {
        Format::of(self.ten_bit, false)
    }
}

/// What a unit amounted to, either codec.
enum Read {
    Nothing,
    Picture,
    FormatChanged,
}

/// The staging texture a read-back goes through, at the size and format of
/// the decoder's textures.
struct Staging {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    texture: Com<ID3D11Texture2D>,
}

/// A view pair the split reads one of the decoder's textures through,
/// found by the texture's address -- which cannot name another texture
/// while it is kept, since the views hold the texture.
type Views = (usize, [Option<Com<ID3D11ShaderResourceView>>; 2]);

/// A decoder built for a stream and what it has handed out, dropped in field
/// order: the surfaces and the views before the decoder.
struct Built {
    shape: Shape,
    /// The coded size the decoder was built at.
    coded: (u32, u32),
    /// Whether it decodes in the runtime's low-latency mode.
    low_latency: bool,
    /// The surfaces handed out, by the readers' slot, until their picture
    /// leaves.
    held: [Option<Surface>; SURFACES],
    /// Units submitted whose picture has not come out: the mark and the
    /// slot.
    pending: [Option<(i64, usize)>; PENDING],
    views: [Option<Views>; VIEWS],
    /// Where the next pair of views goes.
    next_view: usize,
    staging: Option<Staging>,
    decoder: Component,
}

/// AMD's decoder over one device.
pub struct Backend<'a> {
    // Dropped in this order: the decoder and what it handed out, the
    // buffer, then the context they were made in.
    built: Option<Built>,
    buffer: Buffer,
    context: Context<'a>,
    amf: &'a Amf,
    device: &'a Device,
    /// The largest coded picture the caller's planes take.
    ceiling: (u32, u32),
    /// The split and the fence it signals, on a device that has fences.
    split: Option<Split>,
    /// What a sleep on the fence is woken by.
    event: Event,
    codec: Codec,
    /// The declaration's depth, until the first parameter set says.
    ten_bit: bool,
    h264: Box<h264::Stream>,
    hevc: Box<hevc::Stream>,
    /// The mark the last unit submitted carried: its number.
    mark: i64,
    /// The last read-back's wait for the device -- the decode and the copy
    /// -- and its copy out, in microseconds, for the log. For a picture
    /// split into textures, which nothing here waits for, the decode is the
    /// device's own timing of the last one it has finished.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl fmt::Debug for Backend<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("codec", &self.codec)
            .field("shape", &self.built.as_ref().map(|b| b.shape))
            .field("coded", &self.built.as_ref().map(|b| b.coded))
            .finish()
    }
}

/// Ask `decoder` to hand every picture out as soon as its unit is decoded,
/// and, where its runtime has the mode, to decode in its low-latency mode:
/// whether it does.
fn configure(decoder: &Component) -> Result<bool> {
    decoder.set_int(REORDER_MODE, REORDER_LOW_LATENCY)?;
    Ok(decoder.has(LOW_LATENCY_DECODE) && decoder.set_bool(LOW_LATENCY_DECODE, true).is_ok())
}

/// What AMD's decoder decodes on `device`, **asked by building a real
/// decoder per codec and depth and dropping it**, never a list believed; and
/// whether every one of them has the runtime's low-latency mode, which the
/// decoder's place in the automatic order waits on.
pub fn caps(amf: &Amf, device: &Device) -> (Caps, bool) {
    let Ok(context) = amf.context(device) else {
        return (Caps::default(), false);
    };
    let mut low_latency = true;
    let mut builds = |codec: runtime::Codec, layout: Layout| {
        let built = amf.decoder(&context, codec).ok().and_then(|decoder| {
            let fast = configure(&decoder).ok()?;
            decoder.init(layout, PROBE_SIZE.0, PROBE_SIZE.1).ok()?;
            Some(fast)
        });
        if let Some(fast) = built {
            low_latency &= fast;
        }
        built.is_some()
    };
    let caps = Caps {
        h264: builds(runtime::Codec::H264, Layout::Nv12),
        hevc: builds(runtime::Codec::Hevc, Layout::Nv12),
        hevc_10: builds(runtime::Codec::Hevc, Layout::P010),
        hevc_444: false,
        hevc_444_10: false,
    };
    (caps, caps.any() && low_latency)
}

impl<'a> Backend<'a> {
    /// The decoder on `device`, an AMD GPU's, the runtime's context made on
    /// it, and the one buffer of `unit_bytes` every unit is handed over in.
    pub fn new(
        amf: &'a Amf,
        device: &'a Device,
        ceiling: (u32, u32),
        unit_bytes: usize,
    ) -> Result<Self> {
        let context = amf.context(device)?;
        let buffer = context.buffer(unit_bytes)?;
        Ok(Self {
            built: None,
            buffer,
            context,
            amf,
            device,
            ceiling,
            // The split needs the fence to say when its work is done; a
            // device without one hands pictures out by read-back only.
            split: Split::new(device),
            event: Event::new()?,
            codec: Codec::H264,
            ten_bit: false,
            h264: Box::new(h264::Stream::new()),
            hevc: Box::new(hevc::Stream::new()),
            mark: 0,
            decode_us: 0,
            readback_us: 0,
        })
    }

    /// Let every waiting picture out, as at the end of a stream; a test's
    /// need, since a live stream never ends this way.
    pub fn drain(&mut self) {
        self.h264.drain();
        self.hevc.drain();
    }

    /// The layout pictures come back in.
    pub fn format(&self) -> Format {
        self.built.as_ref().map_or(
            Format::of(self.ten_bit && self.codec == Codec::H265, false),
            |b| b.shape.format(),
        )
    }

    /// The size and layout the pictures [`Decoder::take`] hands out have,
    /// once the stream has said: the active parameter set's visible size.
    pub fn output(&self) -> Option<(u32, u32, Format)> {
        let (width, height) = self.visible_and_range().0;
        (width > 0 && height > 0).then_some((width, height, self.format()))
    }

    /// Whether the decoder built decodes in the runtime's low-latency mode;
    /// `None` before one is.
    pub fn low_latency(&self) -> Option<bool> {
        self.built.as_ref().map(|b| b.low_latency)
    }

    /// Whether the device is gone: what a call refused by it, or never made
    /// because something it owns was refused, then is.
    pub fn lost(&self) -> bool {
        self.device.lost()
    }

    /// The fence the split signals, on a device that splits: a picture a
    /// take hands out is finished once this passes the take's value.
    pub fn fence(&self) -> Option<Arc<Fence>> {
        self.split.as_ref().map(Split::fence)
    }

    /// Whether pictures can leave as textures: the device has the split and
    /// the fence it signals.
    pub fn splits(&self) -> bool {
        self.split.is_some()
    }

    /// The fault a failed call is: the device lost, whatever the call said,
    /// when the device says it is gone; `otherwise` when it does not.
    fn fault(&self, otherwise: Fault) -> Fault {
        if self.device.lost() {
            Fault::DeviceLost
        } else {
            otherwise
        }
    }

    /// The decoder for `shape` at `coded`, built if none is; `false` when
    /// the one built differs, which is the caller's format change. A
    /// decoder the runtime will not build is a stream it takes no profile
    /// for, which no keyframe asked for would change.
    fn ensure(&mut self, shape: Shape, coded: (u32, u32)) -> Result<bool> {
        if let Some(built) = &self.built {
            return Ok(built.shape == shape && built.coded == coded);
        }
        if coded.0 > self.ceiling.0 || coded.1 > self.ceiling.1 {
            return Err(Error::TooLarge);
        }
        let decoder = self
            .amf
            .decoder(&self.context, shape.decoder())
            .map_err(|_| Error::NoProfile)?;
        let low_latency = configure(&decoder).map_err(|_| Error::NoProfile)?;
        decoder
            .init(shape.layout(), coded.0, coded.1)
            .map_err(|_| Error::NoProfile)?;
        self.built = Some(Built {
            shape,
            coded,
            low_latency,
            held: [const { None }; SURFACES],
            pending: [None; PENDING],
            views: [const { None }; VIEWS],
            next_view: 0,
            staging: None,
            decoder,
        });
        Ok(true)
    }

    fn decode(&mut self, unit: &[u8]) -> Result<Fed> {
        let read = match self.codec {
            Codec::H264 => match self.h264.read(unit).map_err(|_| Error::Stream)? {
                h264::Read::Nothing => Read::Nothing,
                h264::Read::Picture => Read::Picture,
                h264::Read::FormatChanged => Read::FormatChanged,
            },
            Codec::H265 => match self.hevc.read(unit).map_err(|_| Error::Stream)? {
                hevc::Read::Nothing => Read::Nothing,
                hevc::Read::Picture => Read::Picture,
                hevc::Read::FormatChanged => Read::FormatChanged,
            },
        };
        match read {
            Read::FormatChanged => return Ok(Fed::FormatChanged),
            // Parameter sets alone, or nothing decodable: the runtime reads
            // the stream itself, so it is handed the unit all the same once
            // a decoder exists. Before, there is nowhere for it to go; the
            // first picture's unit carries its parameter sets, as every
            // host's keyframe does.
            Read::Nothing => {
                if self.built.is_some() && !self.submit(unit, None)? {
                    return Ok(Fed::FormatChanged);
                }
                return Ok(self.pending_fed());
            }
            Read::Picture => {}
        }
        let (shape, coded, slot) = match self.staged() {
            Ok(staged) => staged,
            Err(e) => {
                self.abandon();
                return Err(e);
            }
        };
        match self.ensure(shape, coded) {
            Ok(true) => {}
            Ok(false) => {
                self.abandon();
                return Ok(Fed::FormatChanged);
            }
            Err(e) => {
                self.abandon();
                return Err(e);
            }
        }
        match self.submit(unit, Some(slot)) {
            Ok(true) => {}
            Ok(false) => {
                self.abandon();
                return Ok(Fed::FormatChanged);
            }
            Err(e) => {
                self.abandon();
                return Err(e);
            }
        }
        match self.codec {
            Codec::H264 => self.h264.finish().map_err(|_| Error::Stream)?,
            Codec::H265 => self.hevc.finish().map_err(|_| Error::Stream)?,
        }
        Ok(self.pending_fed())
    }

    /// The staged picture's shape, coded size and slot; a stream no decoder
    /// here takes is refused.
    fn staged(&self) -> Result<(Shape, (u32, u32), usize)> {
        match self.codec {
            Codec::H264 => {
                let job = self.h264.job().ok_or(Error::Stream)?;
                // Eight-bit 4:2:0 is the decoder's whole answer for the
                // first codec.
                if job.sps.chroma_format_idc != 1 || job.sps.bit_depth_luma_minus8 != 0 {
                    return Err(Error::NoProfile);
                }
                let shape = Shape {
                    codec: Codec::H264,
                    ten_bit: false,
                };
                let coded = (job.sps.coded_width(), job.sps.coded_height());
                Ok((shape, coded, job.current.slot))
            }
            Codec::H265 => {
                let job = self.hevc.job().ok_or(Error::Stream)?;
                // 4:2:0 at eight and ten bits; full or half chroma, or the
                // range extension's tools on a 4:2:0 stream, has no decoder
                // here, and the base decoder would read such a stream
                // wrongly without an error.
                if job.sps.chroma_format_idc != 1 || job.sps.is_range_extended() {
                    return Err(Error::NoProfile);
                }
                let shape = Shape {
                    codec: Codec::H265,
                    ten_bit: job.sps.bit_depth_luma_minus8 > 0,
                };
                Ok((shape, (job.sps.width, job.sps.height), job.current.slot))
            }
        }
    }

    /// Hand `unit` to the decoder, marked with its number, and keep what it
    /// hands out -- the picture of a unit read as one, for `slot`, at once.
    /// `false` when the decoder says the stream changed size under it, which
    /// the readers see first on every stream here.
    fn submit(&mut self, unit: &[u8], slot: Option<usize>) -> Result<bool> {
        if slot.is_some() {
            self.settle()?;
        }
        let built = self.built.as_mut().ok_or(Error::NoProfile)?;
        self.buffer.fill(unit)?;
        self.mark = self.mark.wrapping_add(1);
        self.buffer.set_pts(self.mark);
        if let Some(slot) = slot {
            // A slot the readers hand out again drops whatever surface it
            // still held: a picture never let out.
            if let Some(held) = built.held.get_mut(slot) {
                *held = None;
            }
            let free = built.pending.iter().position(Option::is_none).or_else(|| {
                // Full only of units whose picture never came: the
                // oldest goes.
                let oldest = built.pending.iter().flatten().map(|(m, _)| *m).min()?;
                built
                    .pending
                    .iter()
                    .position(|p| p.is_some_and(|(m, _)| m == oldest))
            });
            if let Some(entry) = free.and_then(|i| built.pending.get_mut(i)) {
                *entry = Some((self.mark, slot));
            }
        }
        let mut submitted = built.decoder.submit(&self.buffer)?;
        let mut tries = 0;
        while submitted == Submitted::Repeat && tries < REPEAT_TRIES {
            submitted = built.decoder.resubmit()?;
            tries += 1;
        }
        match submitted {
            Submitted::Taken | Submitted::MoreInput => {}
            Submitted::ResolutionChanged => return Ok(false),
            // Its queue full with two pictures unfinished at most, or a unit
            // it would not finish taking: nothing a later unit mends.
            Submitted::Full | Submitted::Repeat => return Err(Error::Stream),
        }
        while let Some(surface) = built.decoder.query()? {
            place(built, surface);
        }
        Ok(true)
    }

    /// Hold the next unit back while more than [`MOST_UNFINISHED`] pictures
    /// split are unfinished on the device -- asleep on the fence, which a
    /// device keeping up with the stream never makes this do.
    fn settle(&self) -> Result<()> {
        let Some(split) = &self.split else {
            return Ok(());
        };
        if split.settle(MOST_UNFINISHED, &self.event, SETTLE_WAIT)? {
            Ok(())
        } else {
            Err(Error::Stream)
        }
    }

    fn abandon(&mut self) {
        match self.codec {
            Codec::H264 => self.h264.abandon(),
            Codec::H265 => self.hevc.abandon(),
        }
    }

    fn pending_fed(&self) -> Fed {
        let ready = match self.codec {
            Codec::H264 => self.h264.dpb.has_output(),
            Codec::H265 => self.hevc.dpb.has_output(),
        };
        if ready {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        }
    }

    /// The next picture to leave, by the readers' order: its slot and its
    /// order count.
    fn next_output(&mut self) -> Option<(usize, i32)> {
        match self.codec {
            Codec::H264 => self.h264.next_output().map(|o| (o.slot, o.poc)),
            Codec::H265 => self.hevc.next_output().map(|o| (o.slot, o.poc)),
        }
    }

    fn taken(&mut self, slot: usize) {
        match self.codec {
            Codec::H264 => self.h264.dpb.taken(slot),
            Codec::H265 => self.hevc.dpb.taken(slot),
        }
    }

    /// The active parameter set's visible size and range.
    fn visible_and_range(&self) -> ((u32, u32), bool) {
        match self.codec {
            Codec::H264 => self
                .h264
                .active_sps()
                .map_or(((0, 0), false), |s| (s.visible(), s.vui.video_full_range)),
            Codec::H265 => self
                .hevc
                .active_sps()
                .map_or(((0, 0), false), |s| (s.visible(), s.video_full_range)),
        }
    }

    fn picture(&self, order: i32) -> Picture {
        let ((width, height), full_range) = self.visible_and_range();
        Picture {
            format: self.format(),
            width,
            height,
            order,
            full_range,
        }
    }

    /// Split the next picture to leave into `planes`, one texture per plane
    /// as the system's interface lays them out, and signal the fence behind
    /// it. Returns the picture and the fence value it is finished at; waits
    /// for nothing. A device without the split refuses as fatal.
    pub fn take_to_textures(
        &mut self,
        planes: [Option<&SharedTexture>; 3],
    ) -> core::result::Result<Option<(Picture, u64)>, Fault> {
        let Some((slot, order)) = self.next_output() else {
            return Ok(None);
        };
        let started = lowlat_common::clock::Time::now();
        // What this thread spends is the submit; the decode's time is the
        // device's own, from the take to the fence passing, read once the
        // device has passed it -- the last timed picture's, by now.
        let timed = match self.split.as_mut() {
            Some(split) => split.take(self.device, &mut self.decode_us),
            None => false,
        };
        let split = self.split_picture(slot, planes, timed);
        self.taken(slot);
        let value = split.map_err(|e| {
            self.fault(match e {
                Error::Device(d3d11::Error::Runtime(_)) | Error::NoProfile => Fault::Fatal,
                _ => Fault::Unrecoverable,
            })
        })?;
        self.readback_us = micros(lowlat_common::clock::elapsed_ms(started));
        Ok(Some((self.picture(order), value)))
    }

    /// The split of `slot`'s picture into `planes`, and the fence value it
    /// is finished at.
    fn split_picture(
        &mut self,
        slot: usize,
        planes: [Option<&SharedTexture>; 3],
        timed: bool,
    ) -> Result<u64> {
        let visible = self.visible_and_range().0;
        let built = self.built.as_mut().ok_or(Error::NoProfile)?;
        let split = self.split.as_mut().ok_or(Error::NoProfile)?;
        let format = built.shape.format();
        let surface = built
            .held
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(Error::Stream)?;
        let texture = surface.texture().cast::<ID3D11Texture2D>();
        if texture.is_null() {
            return Err(Error::Stream);
        }
        let sources = views_for(
            self.device,
            &mut built.views,
            &mut built.next_view,
            format,
            texture,
        )?;
        split.dispatch(self.device, format, sources, planes, visible)?;
        let value = split.signal(self.device, timed)?;
        // Back to the decoder, which decodes into its texture again only
        // behind the split just queued: the device orders the two.
        drop(surface);
        Ok(value)
    }

    /// Read `slot`'s picture into the planes: a copy of its texture into
    /// the staging texture, queued behind its decode, then the staging
    /// texture mapped, the mapping's wait sleeping on the device's progress.
    fn read_back(&mut self, slot: usize, out: &mut Planes<'_>) -> Result<()> {
        let visible = self.visible_and_range().0;
        let built = self.built.as_mut().ok_or(Error::NoProfile)?;
        let format = built.shape.format();
        let surface = built
            .held
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(Error::Stream)?;
        let texture = surface.texture().cast::<ID3D11Texture2D>();
        if texture.is_null() {
            return Err(Error::Stream);
        }
        let (rows, staging) = staging_for(self.device, &mut built.staging, texture)?;
        let context = self.device.context();
        let started = lowlat_common::clock::Time::now();
        // SAFETY: a live context; both are this device's, of one format and
        // size, the source a single texture copied whole.
        unsafe {
            vcall!(
                context,
                CopySubresourceRegion,
                staging.cast::<ID3D11Resource>(),
                0,
                0,
                0,
                0,
                texture.cast::<ID3D11Resource>(),
                0,
                core::ptr::null()
            )
        }
        .ok_or(Error::NoProfile)?;
        // The copy is queued: the surface goes back to the decoder, which
        // decodes into it again only behind the copy.
        drop(surface);
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = zeroed();
        // SAFETY: as above; the output is live.
        let hr = unsafe {
            vcall!(
                context,
                Map,
                staging.cast::<ID3D11Resource>(),
                0,
                D3D11_MAP_READ,
                0,
                &raw mut mapped
            )
        }
        .ok_or(Error::NoProfile)?;
        d3d11::check(hr)?;
        let synced = lowlat_common::clock::Time::now();
        let result = copy_planes(&mapped, rows, format, visible, out);
        // SAFETY: mapped above, unmapped once.
        unsafe { vcall!(context, Unmap, staging.cast::<ID3D11Resource>(), 0) };
        let done = lowlat_common::clock::Time::now();
        self.decode_us = micros(lowlat_common::clock::diff_ms(started, synced));
        self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
        result.map_err(Error::from)
    }
}

/// Put a surface the decoder handed out in the slot of the unit it
/// completes, found by the unit's mark; one no unit asked for is let go.
fn place(built: &mut Built, surface: Surface) {
    let mark = surface.pts();
    let Some(slot) = built
        .pending
        .iter_mut()
        .find(|p| p.is_some_and(|(m, _)| m == mark))
        .and_then(Option::take)
        .map(|(_, slot)| slot)
    else {
        return;
    };
    // Units before it that completed no picture of their own -- a field's
    // first half, say -- have none coming.
    for p in &mut built.pending {
        if p.is_some_and(|(m, _)| m < mark) {
            *p = None;
        }
    }
    if let Some(held) = built.held.get_mut(slot) {
        *held = Some(surface);
    }
}

/// The views the split reads `texture` through, made the first time the
/// decoder hands it out. The decoder's textures are single ones, each read
/// at its only slice; one of several slices is refused rather than read at
/// the wrong one.
fn views_for(
    device: &Device,
    views: &mut [Option<Views>; VIEWS],
    next: &mut usize,
    format: Format,
    texture: *mut ID3D11Texture2D,
) -> Result<[*mut ID3D11ShaderResourceView; 2]> {
    let pointers = |pair: &[Option<Com<ID3D11ShaderResourceView>>; 2]| {
        pair.each_ref()
            .map(|v| v.as_ref().map_or(core::ptr::null_mut(), Com::as_ptr))
    };
    let key = texture.addr();
    if let Some((_, pair)) = views.iter().flatten().find(|(k, _)| *k == key) {
        return Ok(pointers(pair));
    }
    let mut desc: D3D11_TEXTURE2D_DESC = zeroed();
    // SAFETY: a live texture; the output is live.
    unsafe { vcall!(texture, GetDesc, &raw mut desc) };
    if desc.ArraySize != 1 {
        return Err(Error::NoProfile);
    }
    let made = split::source_views(device, format, texture, 0)?;
    let place = views.get_mut(*next % VIEWS).ok_or(Error::TooLarge)?;
    *next = next.wrapping_add(1);
    let (_, pair) = place.insert((key, made));
    Ok(pointers(pair))
}

/// The staging texture a read-back of `texture` goes through, made at its
/// size and format and again when those change; with its rows.
fn staging_for(
    device: &Device,
    kept: &mut Option<Staging>,
    texture: *mut ID3D11Texture2D,
) -> Result<(u32, *mut ID3D11Texture2D)> {
    let mut desc: D3D11_TEXTURE2D_DESC = zeroed();
    // SAFETY: a live texture; the output is live.
    unsafe { vcall!(texture, GetDesc, &raw mut desc) };
    let fits = kept
        .as_ref()
        .is_some_and(|k| (k.width, k.height, k.format) == (desc.Width, desc.Height, desc.Format));
    if !fits {
        *kept = Some(Staging {
            width: desc.Width,
            height: desc.Height,
            format: desc.Format,
            texture: split::staging(device, desc.Format, desc.Width, desc.Height)?,
        });
    }
    let staging = kept.as_ref().ok_or(Error::NoProfile)?;
    Ok((staging.height, staging.texture.as_ptr()))
}

impl Decoder for Backend<'_> {
    fn build(&mut self, header: &VideoHeader) -> core::result::Result<(), Fault> {
        self.codec = header.codec;
        self.ten_bit = header.ten_bit;
        self.h264.reset();
        self.hevc.reset();
        // The decoder itself waits for the first parameter set, which says
        // the size and the depth.
        Ok(())
    }

    fn feed(&mut self, unit: &[u8]) -> core::result::Result<Fed, Fault> {
        match self.decode(unit) {
            Ok(fed) => Ok(fed),
            // A stream the decoder cannot take would be refused again on
            // every keyframe asked for; nothing to ask.
            Err(Error::NoProfile | Error::TooLarge | Error::Device(d3d11::Error::Runtime(_))) => {
                Err(self.fault(Fault::Fatal))
            }
            Err(_) => Err(self.fault(Fault::Unrecoverable)),
        }
    }

    fn take(&mut self, out: &mut Planes<'_>) -> core::result::Result<Option<Picture>, Fault> {
        let Some((slot, order)) = self.next_output() else {
            return Ok(None);
        };
        let read = self.read_back(slot, out);
        self.taken(slot);
        read.map_err(|e| {
            self.fault(match e {
                Error::Device(d3d11::Error::Runtime(_)) => Fault::Fatal,
                _ => Fault::Unrecoverable,
            })
        })?;
        Ok(Some(self.picture(order)))
    }

    fn destroy(&mut self) {
        self.built = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The depth picks the layout the decoder hands out and the format the
    /// caller gets, never eight bits for a ten-bit stream; both codecs map to
    /// the runtime's two decoders.
    #[test]
    fn the_shape_names_its_decoder_and_layout() {
        let eight = Shape {
            codec: Codec::H265,
            ten_bit: false,
        };
        let ten = Shape {
            codec: Codec::H265,
            ten_bit: true,
        };
        assert_eq!(eight.layout(), Layout::Nv12);
        assert_eq!(eight.format(), Format::Nv12);
        assert_eq!(ten.layout(), Layout::P010);
        assert_eq!(ten.format(), Format::P010);
        assert_eq!(ten.decoder(), runtime::Codec::Hevc);
        let h264 = Shape {
            codec: Codec::H264,
            ten_bit: false,
        };
        assert_eq!(h264.decoder(), runtime::Codec::H264);
    }
}
