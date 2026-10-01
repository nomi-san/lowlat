//! Intel's own decoder, through its runtime: the current runtime on a device
//! of the library's own, or the older one into memory of its own; the
//! readers here say which picture leaves when.
//!
//! **The readers stay in front.** The runtime parses the stream itself, so a
//! unit goes to it whole; the readers read it too, for what the runtime does
//! not say before it decodes -- the stream's size, depth and range, a change
//! of format, a unit that cannot be read -- and for the order pictures leave
//! in. The runtime is asked to hand every picture out from the call that
//! decodes it, in decode order, and the readers' picture buffer lets each
//! out in the stream's own order, a picture held until its turn: none at all
//! on a stream that does not reorder, which is every host's. Each unit
//! carries its number in its time stamp, which the picture it completes
//! carries out, so a picture finds its readers' slot however late a runtime
//! hands it out.
//!
//! **Nothing waits for a decode on the current runtime.** It makes its decode
//! calls on the device's context inside the decode call, so the split and a
//! read-back's copy queued on that context afterwards are ordered behind the
//! decode, and the library's fence is signalled behind the split. No more
//! than two split pictures are left unfinished when the next unit goes in:
//! past that the decode thread sleeps on the fence, which only a device
//! fallen behind the stream makes it do. **The older runtime decodes into
//! surfaces of the backend's own memory**, planes only, and a read-back waits
//! for its picture there.
//!
//! **A unit of parameter sets alone is kept**, and given to the readers and to
//! the runtime again whenever a decoder is built, so a stream whose sets
//! travel ahead of the keyframe still builds one.

use core::fmt;
use core::ptr::NonNull;
use core::time::Duration;
use std::sync::Arc;

use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::d3d11::{Com, Device, Event, Fence, SharedTexture};
use lowlat_drivers::ffi::d3d11::{
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, DXGI_FORMAT, ID3D11Device,
    ID3D11Resource, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use lowlat_drivers::ffi::vpl::{
    MFX_BITSTREAM_COMPLETE_FRAME, MFX_CHROMAFORMAT_YUV420, MFX_CHROMAFORMAT_YUV444, MFX_CODEC_AVC,
    MFX_CODEC_HEVC, MFX_ERR_DEVICE_FAILED, MFX_ERR_DEVICE_LOST, MFX_ERR_GPU_HANG, MFX_FOURCC_AYUV,
    MFX_FOURCC_NV12, MFX_FOURCC_P010, MFX_FOURCC_Y410, MFX_IOPATTERN_OUT_SYSTEM_MEMORY,
    MFX_IOPATTERN_OUT_VIDEO_MEMORY, MFX_PICSTRUCT_PROGRESSIVE, MFX_PROFILE_AVC_HIGH,
    MFX_PROFILE_HEVC_MAIN, MFX_PROFILE_HEVC_MAIN10, MFX_PROFILE_HEVC_REXT, mfxBitstream,
    mfxFrameInfo, mfxFrameSurface1, mfxVideoParam,
};
use lowlat_drivers::vcall;
use lowlat_drivers::vpl::{self as runtime, Decoded, Picture as Held, Runtime, Session};

use crate::amf::Pending;
use crate::d3d11;
use crate::split::{self, Split, copy_planes, copy_rows, micros};
use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes, h264, hevc, nal};

/// Surfaces the readers' slots name: what either picture buffer indexes.
const SURFACES: usize = h264::dpb::MAX_FRAMES;
const _: () = assert!(hevc::dpb::MAX_PICTURES <= SURFACES);
/// Views kept, a pair per texture the runtime hands out: it hands a few
/// out over and over.
const VIEWS: usize = 16;
/// Pictures split and not yet finished on the device when the next unit is
/// submitted, at most.
pub const MOST_UNFINISHED: u64 = 2;
/// How long a device fallen behind is slept on before the decoder is given
/// up.
const SETTLE_WAIT: Duration = Duration::from_secs(1);
/// How long a busy device is waited for, a millisecond at a time.
const BUSY_TRIES: u32 = 50;
const BUSY_WAIT: Duration = Duration::from_millis(1);
/// How long the older runtime's read-back waits for its picture.
const SYNC_WAIT_MS: u32 = 1000;
/// Surfaces of its own the older runtime is given beyond what it asks for:
/// the pictures the readers hold for their turn.
const SPARE_SURFACES: usize = 4;
/// The size a decoder is built at to ask whether it builds at all.
const PROBE_SIZE: (u16, u16) = (1280, 720);

/// A header's constant as the sixteen-bit field it is written into. Every
/// one used here is small and positive; one that is not fails the build.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the assertion bounds the value to the field"
)]
const fn word(value: i32) -> u16 {
    assert!(value >= 0 && value <= 0xffff);
    value as u16
}

const VIDEO_MEMORY: u16 = word(MFX_IOPATTERN_OUT_VIDEO_MEMORY);
const SYSTEM_MEMORY: u16 = word(MFX_IOPATTERN_OUT_SYSTEM_MEMORY);
const COMPLETE_FRAME: u16 = word(MFX_BITSTREAM_COMPLETE_FRAME);
const PROGRESSIVE: u16 = word(MFX_PICSTRUCT_PROGRESSIVE);
const YUV420: u16 = word(MFX_CHROMAFORMAT_YUV420);
const YUV444: u16 = word(MFX_CHROMAFORMAT_YUV444);
const AVC_HIGH: u16 = word(MFX_PROFILE_AVC_HIGH);
const HEVC_MAIN: u16 = word(MFX_PROFILE_HEVC_MAIN);
const HEVC_MAIN10: u16 = word(MFX_PROFILE_HEVC_MAIN10);
const HEVC_REXT: u16 = word(MFX_PROFILE_HEVC_REXT);

/// Why a decoder could not be built or a picture could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Intel's runtime refused a call.
    Runtime(runtime::Error),
    /// The device: the split, a read-back, or the device lost.
    Device(d3d11::Error),
    /// A stream no decoder here takes, or a decoder the runtime would not
    /// build.
    NoProfile,
    /// A unit the readers could not read, or a picture the runtime never
    /// handed out.
    Stream,
    /// A picture larger than the caller's planes, or a unit larger than a
    /// bitstream says.
    TooLarge,
    /// The runtime decoded on a device other than the one it was given.
    NotOurs,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(e) => write!(f, "{e}"),
            Self::Device(e) => write!(f, "{e}"),
            Self::NoProfile => f.write_str("Intel decoder takes no stream of this shape"),
            Self::Stream => f.write_str("Intel decoder lost a picture of the stream"),
            Self::TooLarge => f.write_str("picture or unit larger than the decoder holds"),
            Self::NotOurs => f.write_str("Intel decoder decoded on a device of its own"),
        }
    }
}

impl std::error::Error for Error {}

impl From<runtime::Error> for Error {
    fn from(e: runtime::Error) -> Self {
        Self::Runtime(e)
    }
}

impl From<d3d11::Error> for Error {
    fn from(e: d3d11::Error) -> Self {
        Self::Device(e)
    }
}

impl From<lowlat_drivers::d3d11::Error> for Error {
    fn from(e: lowlat_drivers::d3d11::Error) -> Self {
        Self::Device(d3d11::Error::from(e))
    }
}

type Result<T> = core::result::Result<T, Error>;

// SAFETY: every structure this is used for is plain data the runtime's or
// the device's headers define, whose all-zero value is valid and is where a
// fill starts.
fn zeroed<T>() -> T {
    unsafe { core::mem::zeroed() }
}

/// What a decoder is built for: the codec, the depth and the chroma, which
/// the layout pictures are handed out in follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    codec: Codec,
    ten_bit: bool,
    full_chroma: bool,
}

impl Shape {
    const fn format(self) -> Format {
        Format::of(self.ten_bit, self.full_chroma)
    }

    /// The runtime's layout for the shape.
    const fn fourcc(self) -> u32 {
        match (self.ten_bit, self.full_chroma) {
            (false, false) => MFX_FOURCC_NV12 as u32,
            (true, false) => MFX_FOURCC_P010 as u32,
            (false, true) => MFX_FOURCC_AYUV as u32,
            (true, true) => MFX_FOURCC_Y410 as u32,
        }
    }

    const fn codec_id(self) -> u32 {
        match self.codec {
            Codec::H264 => MFX_CODEC_AVC as u32,
            Codec::H265 => MFX_CODEC_HEVC as u32,
        }
    }
}

/// What a unit amounted to, either codec.
enum Read {
    Nothing,
    Picture,
    FormatChanged,
}

/// The staging texture a read-back goes through, at the size and format of
/// the runtime's textures.
struct Staging {
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    texture: Com<ID3D11Texture2D>,
}

/// A view pair the split reads one of the runtime's textures through, found
/// by the texture's address -- which cannot name another texture while it is
/// kept, since the views hold the texture.
type Views = (usize, [Option<Com<ID3D11ShaderResourceView>>; 2]);

/// The older runtime's surfaces: descriptions over one block of memory,
/// made with the decoder and never moved while it lives. **Reached through
/// raw pointers alone, never a reference**: the runtime keeps pointers into
/// both while it lives, and its threads write the descriptions' lock counts.
struct Pool {
    surfaces: NonNull<[mfxFrameSurface1]>,
    memory: NonNull<[u8]>,
    /// Each surface's bytes in the block, a row's, and its rows of luma, as
    /// the pool laid them out -- never as a description says, which the
    /// runtime writes.
    frame: usize,
    pitch: usize,
    rows: usize,
}

impl Pool {
    /// The pool's surface at `index`.
    fn surface(&self, index: usize) -> Option<*mut mfxFrameSurface1> {
        (index < self.surfaces.len()).then(|| {
            self.surfaces
                .as_ptr()
                .cast::<mfxFrameSurface1>()
                .wrapping_add(index)
        })
    }

    /// The index of `surface` among the pool's, if it is one.
    fn index_of(&self, surface: *const mfxFrameSurface1) -> Option<usize> {
        let first = self.surfaces.as_ptr().cast::<mfxFrameSurface1>().addr();
        let offset = surface.addr().checked_sub(first)?;
        let size = core::mem::size_of::<mfxFrameSurface1>();
        let index = offset / size;
        (offset % size == 0 && index < self.surfaces.len()).then_some(index)
    }

    /// The luma and chroma rows of the surface at `index`.
    ///
    /// # Safety
    ///
    /// The runtime has finished decoding into the surface and does not
    /// write it while the planes are borrowed.
    unsafe fn planes(&self, index: usize) -> Option<(&[u8], &[u8])> {
        let luma = self.pitch * self.rows;
        let start = index.checked_mul(self.frame)?;
        if start + luma + luma / 2 > self.memory.len() {
            return None;
        }
        let y = self.memory.as_ptr().cast::<u8>().wrapping_add(start);
        // SAFETY: inside the block, checked above, which lives as long as
        // the pool; the caller says nothing writes it meanwhile.
        unsafe {
            Some((
                core::slice::from_raw_parts(y, luma),
                core::slice::from_raw_parts(y.add(luma), luma / 2),
            ))
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: both were made by `Box::into_raw` and are freed once, here,
        // after the decoder that held pointers into them was closed.
        unsafe {
            drop(Box::from_raw(self.surfaces.as_ptr()));
            drop(Box::from_raw(self.memory.as_ptr()));
        }
    }
}

/// A decoder built for a stream and what it has handed out.
struct Built<'a> {
    shape: Shape,
    /// The coded size the decoder was built at.
    coded: (u32, u32),
    /// The pictures handed out, by the readers' slot, until they leave.
    held: [Option<Held<'a>>; SURFACES],
    pending: Pending,
    /// Whether the first picture's texture was found to be the device's.
    checked: bool,
    views: [Option<Views>; VIEWS],
    /// Where the next pair of views goes.
    next_view: usize,
    staging: Option<Staging>,
    pool: Option<Pool>,
}

/// Intel's decoder in one session of its runtime.
pub struct Backend<'a> {
    built: Option<Built<'a>>,
    session: &'a Session<'a>,
    runtime: Runtime,
    /// The current runtime's device; none for the older one's.
    device: Option<&'a Device>,
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
    /// The last unit of parameter sets alone, and its codec: read again by
    /// the readers at each build, and given to a decoder built for a
    /// picture whose own unit carries none.
    sets: Vec<u8>,
    sets_codec: Option<Codec>,
    /// The number the last unit submitted carried.
    mark: i64,
    /// The last read-back's wait -- for a picture split into textures, the
    /// device's own timing of the last one it has finished -- and its copy
    /// out, in microseconds, for the log.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl fmt::Debug for Backend<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("runtime", &self.runtime)
            .field("codec", &self.codec)
            .field("shape", &self.built.as_ref().map(|b| b.shape))
            .field("coded", &self.built.as_ref().map(|b| b.coded))
            .finish()
    }
}

/// A decoder's parameters made by hand for `shape` at `size`, as a stream's
/// parameter sets would give them: what a decoder is built from to ask
/// whether it builds.
fn param_for(shape: Shape, (width, height): (u16, u16), memory: u16) -> mfxVideoParam {
    let mut param: mfxVideoParam = zeroed();
    param.IOPattern = memory;
    // SAFETY: the decoder's half of the union, plain data all zero until
    // filled here.
    let mfx = unsafe { &mut param.__bindgen_anon_1.mfx };
    mfx.CodecId = shape.codec_id();
    mfx.CodecProfile = match (shape.codec, shape.ten_bit, shape.full_chroma) {
        (Codec::H264, _, _) => AVC_HIGH,
        (Codec::H265, _, true) => HEVC_REXT,
        (Codec::H265, true, false) => HEVC_MAIN10,
        (Codec::H265, false, false) => HEVC_MAIN,
    };
    let info: &mut mfxFrameInfo = &mut mfx.FrameInfo;
    info.FourCC = shape.fourcc();
    info.ChromaFormat = if shape.full_chroma { YUV444 } else { YUV420 };
    let depth = if shape.ten_bit { 10 } else { 8 };
    info.BitDepthLuma = depth;
    info.BitDepthChroma = depth;
    // Ten bits in the high bits of each sixteen, for the planar layout.
    info.Shift = u16::from(shape.ten_bit && !shape.full_chroma);
    info.PicStruct = PROGRESSIVE;
    info.__bindgen_anon_1.__bindgen_anon_1.Width = width;
    info.__bindgen_anon_1.__bindgen_anon_1.Height = height;
    info.__bindgen_anon_1.__bindgen_anon_1.CropW = width;
    info.__bindgen_anon_1.__bindgen_anon_1.CropH = height;
    param
}

/// What Intel's decoder decodes in `session`, **asked by building a real
/// decoder per codec, depth and chroma and dropping it**, never a list
/// believed. The older runtime's is asked for four-two-zero alone, which is
/// all it hands out.
pub fn caps(session: &Session<'_>, runtime: Runtime) -> Caps {
    let memory = match runtime {
        Runtime::Current => VIDEO_MEMORY,
        Runtime::Older => SYSTEM_MEMORY,
    };
    let builds = |codec, ten_bit, full_chroma| {
        let shape = Shape {
            codec,
            ten_bit,
            full_chroma,
        };
        let mut param = param_for(shape, PROBE_SIZE, memory);
        // SAFETY: parameters made here, no extension buffer attached.
        let built = unsafe { session.init(&mut param) }.is_ok();
        session.close_decoder();
        built
    };
    let full = runtime == Runtime::Current;
    Caps {
        h264: builds(Codec::H264, false, false),
        hevc: builds(Codec::H265, false, false),
        hevc_10: builds(Codec::H265, true, false),
        hevc_444: full && builds(Codec::H265, false, true),
        hevc_444_10: full && builds(Codec::H265, true, true),
    }
}

/// What a decoder refused at its build is: the device lost, failed or hung
/// stays the runtime's word, which the lost device's route takes; anything
/// else is a decoder the runtime will not build, which the next keyframe
/// would meet again.
fn built_error(error: runtime::Error) -> Error {
    match error {
        runtime::Error::Status(MFX_ERR_DEVICE_LOST | MFX_ERR_DEVICE_FAILED | MFX_ERR_GPU_HANG) => {
            Error::Runtime(error)
        }
        _ => Error::NoProfile,
    }
}

/// The readers' slot the picture handed out of the call that submitted the
/// unit marked `own` goes to: the unit its stamp names, while that unit
/// still waits. **A stamp naming no unit this decoder submitted** -- which a
/// runtime handing pictures out in decode order may write -- is the call's
/// own unit's, the call being the unit then; a stamp naming an earlier unit
/// whose slot has since been handed out again is a picture let go.
fn slot_for(pending: &mut Pending, stamp: i64, own: i64) -> Option<usize> {
    if (1..=own).contains(&stamp) {
        pending.complete(stamp)
    } else {
        pending.complete(own)
    }
}

/// A bitstream over `unit`, one whole unit, marked `mark`.
fn bitstream(unit: &[u8], mark: i64) -> Result<mfxBitstream> {
    let len = u32::try_from(unit.len()).map_err(|_| Error::TooLarge)?;
    let mut bitstream: mfxBitstream = zeroed();
    // The runtime reads a unit and never writes it.
    bitstream.Data = unit.as_ptr().cast_mut();
    bitstream.DataLength = len;
    bitstream.MaxLength = len;
    bitstream.DataFlag = COMPLETE_FRAME;
    bitstream.TimeStamp = u64::from_ne_bytes(mark.to_ne_bytes());
    Ok(bitstream)
}

/// Whether `unit` holds a sequence parameter set of `codec`.
fn carries_sets(codec: Codec, unit: &[u8]) -> bool {
    nal::Units::new(unit).any(|u| {
        let Some(&header) = u.bytes.first() else {
            return false;
        };
        match codec {
            Codec::H264 => header & 0x1f == 7,
            Codec::H265 => (header >> 1) & 0x3f == 33,
        }
    })
}

impl<'a> Backend<'a> {
    /// The decoder in `session`, of `runtime`; `device` the current
    /// runtime's, the one the session decodes on, and none for the older
    /// one; `unit_bytes` the largest unit that comes.
    pub fn new(
        session: &'a Session<'a>,
        runtime: Runtime,
        device: Option<&'a Device>,
        ceiling: (u32, u32),
        unit_bytes: usize,
    ) -> Result<Self> {
        if (runtime == Runtime::Current) != device.is_some() {
            return Err(Error::NoProfile);
        }
        Ok(Self {
            built: None,
            session,
            runtime,
            device,
            ceiling,
            // The split needs the fence to say when its work is done; a
            // device without one hands pictures out by read-back only.
            split: device.and_then(Split::new),
            event: Event::new()?,
            codec: Codec::H264,
            ten_bit: false,
            h264: Box::new(h264::Stream::new()),
            hevc: Box::new(hevc::Stream::new()),
            sets: Vec::with_capacity(unit_bytes),
            sets_codec: None,
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

    /// Whether the device is gone.
    pub fn lost(&self) -> bool {
        self.device.is_some_and(Device::lost)
    }

    /// The fence the split signals, on a device that splits.
    pub fn fence(&self) -> Option<Arc<Fence>> {
        self.split.as_ref().map(Split::fence)
    }

    /// Whether pictures can leave as textures: the current runtime on a
    /// device with the split and the fence it signals.
    pub fn splits(&self) -> bool {
        self.split.is_some()
    }

    /// The fault a failed call is: the device lost when the device says it
    /// is gone or the runtime says it lost it; `otherwise` else.
    fn fault(&self, error: Error, otherwise: Fault) -> Fault {
        let runtime_lost = matches!(
            error,
            Error::Runtime(runtime::Error::Status(
                MFX_ERR_DEVICE_LOST | MFX_ERR_DEVICE_FAILED | MFX_ERR_GPU_HANG
            ))
        );
        if runtime_lost || self.lost() {
            Fault::DeviceLost
        } else {
            otherwise
        }
    }

    fn memory(&self) -> u16 {
        match self.runtime {
            Runtime::Current => VIDEO_MEMORY,
            Runtime::Older => SYSTEM_MEMORY,
        }
    }

    /// The decoder for `shape` at `coded`, built from the parameter sets in
    /// `unit` or, failing them, the ones kept; `false` when the one built
    /// differs, which is the caller's format change.
    fn ensure(&mut self, shape: Shape, coded: (u32, u32), unit: &[u8]) -> Result<bool> {
        if let Some(built) = &self.built {
            return Ok(built.shape == shape && built.coded == coded);
        }
        if coded.0 > self.ceiling.0 || coded.1 > self.ceiling.1 {
            return Err(Error::TooLarge);
        }
        let from_unit = self.session.header(unit, shape.codec_id())?;
        let found = from_unit.is_some();
        let mut param = match from_unit {
            Some(param) => param,
            None if self.sets_codec == Some(shape.codec) => self
                .session
                .header(&self.sets, shape.codec_id())?
                .ok_or(Error::Stream)?,
            None => return Err(Error::Stream),
        };
        // SAFETY: the decoder's half of the union, which the header filled.
        let fourcc = unsafe { param.__bindgen_anon_1.mfx.FrameInfo.FourCC };
        if fourcc != shape.fourcc() {
            return Err(Error::NoProfile);
        }
        param.IOPattern = self.memory();
        // Five in flight is the runtime's own default; asked for one, it
        // answers busy on nearly every unit decoded back to back.
        param.AsyncDepth = 0;
        // Every picture out of the call that decodes it.
        param
            .__bindgen_anon_1
            .mfx
            .__bindgen_anon_1
            .__bindgen_anon_2
            .DecodedOrder = 1;
        let pool = match self.runtime {
            Runtime::Current => None,
            Runtime::Older => Some(self.pool(&mut param, shape)?),
        };
        // SAFETY: the parameters the header filled, no extension buffer
        // attached.
        unsafe { self.session.init(&mut param) }.map_err(built_error)?;
        self.built = Some(Built {
            shape,
            coded,
            held: [const { None }; SURFACES],
            pending: Pending::new(),
            checked: false,
            views: [const { None }; VIEWS],
            next_view: 0,
            staging: None,
            pool,
        });
        // A stream whose sets travelled ahead of this picture gives them to
        // the decoder first.
        if !found {
            let sets = core::mem::take(&mut self.sets);
            let submitted = self.submit(&sets, None);
            self.sets = sets;
            submitted?;
        }
        Ok(true)
    }

    /// The older runtime's surfaces for a decoder of `param`: what it asks
    /// for and the readers' spare, in one block.
    fn pool(&self, param: &mut mfxVideoParam, shape: Shape) -> Result<Pool> {
        // SAFETY: the parameters the header filled, no extension buffer
        // attached.
        let request = unsafe { self.session.surfaces(param) }?;
        let count = usize::from(request.NumFrameSuggested) + SPARE_SURFACES;
        // SAFETY: the decoder's half of the union, which the header filled.
        let info = unsafe { param.__bindgen_anon_1.mfx.FrameInfo };
        // SAFETY: the size half of the union, which the header filled.
        let (width, height) = unsafe {
            (
                usize::from(info.__bindgen_anon_1.__bindgen_anon_1.Width),
                usize::from(info.__bindgen_anon_1.__bindgen_anon_1.Height),
            )
        };
        let sample = shape.format().sample();
        let pitch = (width * sample).next_multiple_of(64);
        let frame = pitch * (height + height / 2);
        let pitch16 = u16::try_from(pitch).map_err(|_| Error::TooLarge)?;
        let memory = NonNull::from(Box::leak(vec![0u8; frame * count].into_boxed_slice()));
        let mut surfaces: Box<[mfxFrameSurface1]> = (0..count).map(|_| zeroed()).collect();
        let block = memory.as_ptr().cast::<u8>();
        for (i, surface) in surfaces.iter_mut().enumerate() {
            let y = block.wrapping_add(i * frame);
            surface.Info = info;
            surface.Data.__bindgen_anon_2.Pitch = pitch16;
            surface.Data.__bindgen_anon_3.Y = y;
            // The chroma rows follow the luma rows in the frame's part of the
            // block, which holds both.
            surface.Data.__bindgen_anon_4.UV = y.wrapping_add(pitch * height);
        }
        // No reference to either is made again: the runtime holds pointers
        // into both from here on.
        let surfaces = NonNull::from(Box::leak(surfaces));
        Ok(Pool {
            surfaces,
            memory,
            frame,
            pitch,
            rows: height,
        })
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
            // Parameter sets alone, or nothing decodable: kept when they are
            // sets, and handed to a decoder that exists, since the runtime
            // reads the stream itself.
            Read::Nothing => {
                if carries_sets(self.codec, unit) && unit.len() <= self.sets.capacity() {
                    self.sets.clear();
                    self.sets.extend_from_slice(unit);
                    self.sets_codec = Some(self.codec);
                }
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
        match self.ensure(shape, coded, unit) {
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
                // Eight-bit 4:2:0 is the runtime's whole answer for the first
                // codec.
                if job.sps.chroma_format_idc != 1 || job.sps.bit_depth_luma_minus8 != 0 {
                    return Err(Error::NoProfile);
                }
                let shape = Shape {
                    codec: Codec::H264,
                    ten_bit: false,
                    full_chroma: false,
                };
                let coded = (job.sps.coded_width(), job.sps.coded_height());
                Ok((shape, coded, job.current.slot))
            }
            Codec::H265 => {
                let job = self.hevc.job().ok_or(Error::Stream)?;
                // 4:2:0 and 4:4:4 at eight and ten bits; half chroma, or a
                // depth past ten, has no layout here.
                let full_chroma = match job.sps.chroma_format_idc {
                    1 => false,
                    3 => true,
                    _ => return Err(Error::NoProfile),
                };
                if job.sps.bit_depth_luma_minus8 > 2 {
                    return Err(Error::NoProfile);
                }
                let shape = Shape {
                    codec: Codec::H265,
                    ten_bit: job.sps.bit_depth_luma_minus8 > 0,
                    full_chroma,
                };
                Ok((shape, (job.sps.width, job.sps.height), job.current.slot))
            }
        }
    }

    /// Hand `unit` to the decoder, marked with its number, and keep the
    /// picture it hands out in the slot of the unit it completes. `false`
    /// when the runtime says the stream changed under it, which the readers
    /// see first on every stream here.
    fn submit(&mut self, unit: &[u8], slot: Option<usize>) -> Result<bool> {
        if slot.is_some() {
            self.settle()?;
        }
        self.mark = self.mark.wrapping_add(1);
        let mark = self.mark;
        let session = self.session;
        let device = self.device;
        let built = self.built.as_mut().ok_or(Error::NoProfile)?;
        if let Some(slot) = slot {
            // A slot the readers hand out again drops whatever picture it
            // still held: one never let out.
            if let Some(held) = built.held.get_mut(slot) {
                *held = None;
            }
            built.pending.expect(mark, slot);
        }
        let mut bitstream = bitstream(unit, mark)?;
        let mut out = false;
        for _ in 0..BUSY_TRIES {
            let work = match &built.pool {
                Some(pool) => free_surface(pool, &built.held).ok_or(Error::Stream)?,
                None => core::ptr::null_mut(),
            };
            // SAFETY: the work surface is null or one of the pool's, which
            // lives, unmoved, as long as the decoder does and is dropped only
            // after it is closed; it is neither locked by the runtime nor
            // held here.
            match unsafe { session.decode(&mut bitstream, work) }? {
                Decoded::Picture(picture) => {
                    place(built, device, picture, mark)?;
                    out = true;
                    if bitstream.DataLength == 0 {
                        return Ok(true);
                    }
                }
                // The unit's picture is out, or none was asked of it: what
                // is left of it -- filler, say, behind the last slice --
                // completes no picture.
                Decoded::MoreData if out || bitstream.DataLength == 0 || slot.is_none() => {
                    return Ok(true);
                }
                Decoded::Incompatible => return Ok(false),
                // A warning and nothing out: the rest at once.
                Decoded::Warning => {}
                // A picture's unit not taken: the runtime has every surface
                // of its own in use -- it takes one back only once its
                // scheduler notes the decode done, a millisecond or two
                // behind the device, so units fed faster than that meet it
                // -- or the device is busy. A moment, then again.
                Decoded::MoreData | Decoded::Busy | Decoded::MoreSurface => {
                    std::thread::sleep(BUSY_WAIT);
                }
            }
        }
        Err(Error::Stream)
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
    /// for nothing. A backend without the split refuses as fatal.
    pub fn take_to_textures(
        &mut self,
        planes: [Option<&SharedTexture>; 3],
    ) -> core::result::Result<Option<(Picture, u64)>, Fault> {
        let Some((slot, order)) = self.next_output() else {
            return Ok(None);
        };
        let started = lowlat_common::clock::Time::now();
        let timed = match (self.split.as_mut(), self.device) {
            (Some(split), Some(device)) => split.take(device, &mut self.decode_us),
            _ => false,
        };
        let split = self.split_picture(slot, planes, timed);
        self.taken(slot);
        let value = split.map_err(|e| {
            self.fault(
                e,
                match e {
                    Error::Device(d3d11::Error::Runtime(_)) | Error::NoProfile | Error::NotOurs => {
                        Fault::Fatal
                    }
                    _ => Fault::Unrecoverable,
                },
            )
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
        let device = self.device.ok_or(Error::NoProfile)?;
        let built = self.built.as_mut().ok_or(Error::NoProfile)?;
        let split = self.split.as_mut().ok_or(Error::NoProfile)?;
        let format = built.shape.format();
        let picture = built
            .held
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(Error::Stream)?;
        let texture = picture.texture().cast::<ID3D11Texture2D>();
        if texture.is_null() {
            return Err(Error::Stream);
        }
        let sources = views_for(
            device,
            &mut built.views,
            &mut built.next_view,
            format,
            texture,
        )?;
        split.dispatch(device, format, sources, planes, visible)?;
        let value = split.signal(device, timed)?;
        // Back to the runtime, which decodes into its texture again only
        // behind the split just queued: the device orders the two.
        drop(picture);
        Ok(value)
    }

    /// Read `slot`'s picture into the planes: from the current runtime's
    /// texture through a staging copy queued behind its decode, the
    /// mapping's wait sleeping on the device's progress; from the older
    /// runtime's surface once the runtime says it is decoded.
    fn read_back(&mut self, slot: usize, out: &mut Planes<'_>) -> Result<()> {
        let visible = self.visible_and_range().0;
        let session = self.session;
        let device = self.device;
        let built = self.built.as_mut().ok_or(Error::NoProfile)?;
        let format = built.shape.format();
        let picture = built
            .held
            .get_mut(slot)
            .and_then(Option::take)
            .ok_or(Error::Stream)?;
        let Some(device) = device else {
            let started = lowlat_common::clock::Time::now();
            if !session.sync(&picture, SYNC_WAIT_MS)? {
                return Err(Error::Stream);
            }
            let synced = lowlat_common::clock::Time::now();
            let result = match &built.pool {
                Some(pool) => copy_surface(&picture, pool, format, visible, out),
                None => Err(Error::NoProfile),
            };
            let done = lowlat_common::clock::Time::now();
            self.decode_us = micros(lowlat_common::clock::diff_ms(started, synced));
            self.readback_us = micros(lowlat_common::clock::diff_ms(synced, done));
            return result;
        };
        let texture = picture.texture().cast::<ID3D11Texture2D>();
        if texture.is_null() {
            return Err(Error::Stream);
        }
        let (rows, staging) = staging_for(device, &mut built.staging, texture)?;
        let context = device.context();
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
        // The copy is queued: the picture goes back to the runtime, which
        // decodes into it again only behind the copy.
        drop(picture);
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

    /// Drop the decoder: the pictures held go back to the runtime first,
    /// then the decoder is closed, and only then the older runtime's
    /// surfaces, which it reads until it is closed.
    fn teardown(&mut self) {
        if let Some(mut built) = self.built.take() {
            built.held = [const { None }; SURFACES];
            self.session.close_decoder();
            drop(built);
        }
    }
}

impl Drop for Backend<'_> {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// A surface of the pool's the runtime may decode into: neither locked by
/// it nor held here for its turn. Its lock count is read as the atomic it
/// is to the runtime's threads, which write it.
fn free_surface(pool: &Pool, held: &[Option<Held<'_>>]) -> Option<*mut mfxFrameSurface1> {
    (0..pool.surfaces.len())
        .filter_map(|i| pool.surface(i))
        .find(|&surface| {
            // SAFETY: a surface of the pool's, live and in place as long as
            // the pool; the count is a sixteen-bit field, aligned for one,
            // which the runtime changes only atomically.
            let locked =
                unsafe { core::sync::atomic::AtomicU16::from_ptr(&raw mut (*surface).Data.Locked) }
                    .load(core::sync::atomic::Ordering::Acquire);
            locked == 0
                && !held
                    .iter()
                    .flatten()
                    .any(|h| core::ptr::eq(h.surface(), surface))
        })
}

/// Put a picture the runtime handed out of the call that submitted the unit
/// marked `own` in the slot of the unit it completes ([`slot_for`]); one no
/// unit waits for is let go. The first picture of a decoder on a device is
/// checked to be that device's: a runtime that fell back to a device of its
/// own is refused.
fn place<'a>(
    built: &mut Built<'a>,
    device: Option<&Device>,
    picture: Held<'a>,
    own: i64,
) -> Result<()> {
    if let Some(device) = device
        && !built.checked
    {
        if !is_ours(device, picture.texture().cast()) {
            return Err(Error::NotOurs);
        }
        built.checked = true;
    }
    let stamp = i64::from_ne_bytes(picture.timestamp().to_ne_bytes());
    let Some(slot) = slot_for(&mut built.pending, stamp, own) else {
        return Ok(());
    };
    if let Some(held) = built.held.get_mut(slot) {
        *held = Some(picture);
    }
    Ok(())
}

/// Whether `texture` is a texture of `device`.
fn is_ours(device: &Device, texture: *mut ID3D11Texture2D) -> bool {
    if texture.is_null() {
        return false;
    }
    let mut owner: *mut ID3D11Device = core::ptr::null_mut();
    // SAFETY: a live texture; the output is a local, handed a reference.
    unsafe { vcall!(texture, GetDevice, &raw mut owner) };
    // SAFETY: the reference the call handed over, released on drop.
    let owner = unsafe { Com::from_raw(owner) };
    owner.is_some_and(|o| o.as_ptr() == device.device())
}

/// The older runtime's picture, from its surface's planes into `out`: the
/// planes as the pool laid them out, whatever the surface's description
/// says by now.
fn copy_surface(
    picture: &Held<'_>,
    pool: &Pool,
    format: Format,
    (width, height): (u32, u32),
    out: &mut Planes<'_>,
) -> Result<()> {
    if format.full_chroma() {
        return Err(Error::NoProfile);
    }
    let index = pool.index_of(picture.surface()).ok_or(Error::Stream)?;
    // SAFETY: the runtime has said the picture is decoded, and holds it as a
    // reference at most, which it reads and does not write.
    let (luma, chroma) = unsafe { pool.planes(index) }.ok_or(Error::TooLarge)?;
    let pitch = pool.pitch;
    let width = usize::try_from(width).map_err(|_| Error::TooLarge)?;
    let height = usize::try_from(height).map_err(|_| Error::TooLarge)?;
    let row_bytes = width * format.sample();
    copy_rows(luma, 0, pitch, out.y, out.y_pitch, row_bytes, height)?;
    copy_rows(
        chroma,
        0,
        pitch,
        out.uv,
        out.uv_pitch,
        row_bytes,
        height.div_ceil(2),
    )?;
    Ok(())
}

/// The views the split reads `texture` through, made the first time the
/// runtime hands it out. The runtime's textures are single ones; one of
/// several slices is refused rather than read at the wrong one.
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
    // The copy reads the texture's first slice: one of several is refused.
    if desc.ArraySize != 1 {
        return Err(Error::NoProfile);
    }
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
        // The sets kept, read again, so a decoder is built from them if the
        // keyframe that follows carries none.
        if self.sets_codec == Some(header.codec) {
            let _ = match self.codec {
                Codec::H264 => self.h264.read(&self.sets).map(|_| ()),
                Codec::H265 => self.hevc.read(&self.sets).map(|_| ()),
            };
        }
        // The decoder itself waits for the first picture, whose parameter
        // sets say the size and the depth.
        Ok(())
    }

    fn feed(&mut self, unit: &[u8]) -> core::result::Result<Fed, Fault> {
        match self.decode(unit) {
            Ok(fed) => Ok(fed),
            // A stream the decoder cannot take would be refused again on
            // every keyframe asked for; nothing to ask.
            Err(
                e @ (Error::NoProfile
                | Error::TooLarge
                | Error::NotOurs
                | Error::Device(d3d11::Error::Runtime(_))),
            ) => Err(self.fault(e, Fault::Fatal)),
            Err(e) => Err(self.fault(e, Fault::Unrecoverable)),
        }
    }

    fn take(&mut self, out: &mut Planes<'_>) -> core::result::Result<Option<Picture>, Fault> {
        let Some((slot, order)) = self.next_output() else {
            return Ok(None);
        };
        let read = self.read_back(slot, out);
        self.taken(slot);
        read.map_err(|e| {
            self.fault(
                e,
                match e {
                    Error::Device(d3d11::Error::Runtime(_)) | Error::NoProfile => Fault::Fatal,
                    _ => Fault::Unrecoverable,
                },
            )
        })?;
        Ok(Some(self.picture(order)))
    }

    fn destroy(&mut self) {
        self.teardown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The depth and the chroma pick the runtime's layout and the format the
    /// caller gets, never eight bits for a ten-bit stream.
    #[test]
    fn the_shape_names_its_layout() {
        let shape = |ten_bit, full_chroma| Shape {
            codec: Codec::H265,
            ten_bit,
            full_chroma,
        };
        assert_eq!(shape(false, false).fourcc(), MFX_FOURCC_NV12 as u32);
        assert_eq!(shape(true, false).fourcc(), MFX_FOURCC_P010 as u32);
        assert_eq!(shape(false, true).fourcc(), MFX_FOURCC_AYUV as u32);
        assert_eq!(shape(true, true).fourcc(), MFX_FOURCC_Y410 as u32);
        assert_eq!(shape(true, true).format(), Format::Yuv444_16);
        let param = param_for(shape(true, false), PROBE_SIZE, 0);
        // SAFETY: the decoder's half of the union, just filled.
        let info = unsafe { param.__bindgen_anon_1.mfx.FrameInfo };
        assert_eq!((info.BitDepthLuma, info.Shift), (10, 1));
    }

    /// **A device lost at a build is the lost device's, not a stream no
    /// decoder takes**: the older runtime has no device to ask, so only the
    /// runtime's word says so.
    #[test]
    fn a_device_lost_at_a_build_is_not_a_refused_stream() {
        for status in [MFX_ERR_DEVICE_LOST, MFX_ERR_DEVICE_FAILED, MFX_ERR_GPU_HANG] {
            let error = runtime::Error::Status(status);
            assert_eq!(built_error(error), Error::Runtime(error));
        }
        assert_eq!(built_error(runtime::Error::Status(-3)), Error::NoProfile);
    }

    /// **A stamp naming no unit submitted places the picture by its call**,
    /// the call being the unit when pictures leave in decode order; a stamp
    /// naming an earlier unit whose slot was handed out again places nothing.
    #[test]
    fn a_picture_whose_stamp_names_no_unit_goes_to_its_calls_slot() {
        let mut pending = Pending::new();
        pending.expect(5, 3);
        assert_eq!(slot_for(&mut pending, 0, 5), Some(3));
        pending.expect(6, 4);
        assert_eq!(slot_for(&mut pending, i64::MIN, 6), Some(4));
        pending.expect(7, 2);
        assert_eq!(slot_for(&mut pending, 7, 7), Some(2));
        // Unit 8's slot is unit 9's too: 8's picture, late, finds none.
        pending.expect(8, 1);
        pending.expect(9, 1);
        assert_eq!(slot_for(&mut pending, 8, 9), None);
        assert_eq!(slot_for(&mut pending, 9, 9), Some(1));
    }

    /// A unit of parameter sets is told from a picture's, either codec.
    #[test]
    fn a_unit_of_sets_is_told_apart() {
        assert!(carries_sets(Codec::H264, &[0, 0, 1, 0x67, 0xaa]));
        assert!(!carries_sets(Codec::H264, &[0, 0, 1, 0x65, 0xaa]));
        assert!(carries_sets(Codec::H265, &[0, 0, 1, 0x42, 0x01, 0xaa]));
        assert!(!carries_sets(Codec::H265, &[0, 0, 1, 0x26, 0x01, 0xaa]));
    }
}
