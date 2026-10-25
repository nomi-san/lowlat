//! Intel's encoder, on the D3D11 device of the output it encodes
//! (docs/impl-plan-windows.md W2.8).
//!
//! **What it reads is the host's conversion as it wrote it** -- two planes of
//! YUV at the session's depth, or at full chroma the packed words, AYUV at
//! eight bits and Y410 at ten -- each texture made a surface once and handed
//! over with no copy ([05 §3](../../../docs/05-host.md)); or the generator's
//! frames, written from ordinary memory into textures of its own.
//!
//! **The runtime's own wait is the collect.** It submits each picture from a
//! thread of its own, unordered against the device's queue, so nothing on the
//! device says a picture is done: a collect asks the runtime without waiting,
//! and [`Encoder::wait_finished`] sleeps in the same call, on the loop's
//! thread. The encoder starts no thread of its own. **That thread finishes
//! the picture on the device's context**, so the frame source on the device
//! is told while a picture is in the engine ([`Engine`]), to hold the device
//! in no wait of its own meanwhile.
//!
//! **The engine takes one picture at a time**, at the depth that hands each
//! picture out as soon as it is encoded: a second handed over while one is in
//! it is refused, as back pressure, so one bitstream and one of the
//! generator's inputs serve. **A change of rate waits for the engine to
//! empty**: the runtime makes it only once the picture in the engine is done,
//! which would hold the loop up to a whole encode, so one asked for meanwhile
//! is made before the next picture goes in.
//!
//! **The rate's buffer is fixed when the encoder is built**, at six pictures
//! of the rate it is built with, the session's ceiling: a live change moves
//! the target, the peak and the largest picture but never the buffer, and
//! under five pictures of buffer the first codec's rate stays where a cut left
//! it. Each picture is bounded instead, at the largest-picture limit, moved
//! with every change; a change is made from the parameters the encoder was
//! built with and forbids a new sequence, so it is never a keyframe.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::sync::Arc;

use lowlat_drivers::d3d11::{Com, Device};
use lowlat_drivers::vcall;
use lowlat_drivers::vpl::{self as runtime, Encoded, Pending, Session};

use crate::ffi::d3d11::{
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, DXGI_FORMAT, DXGI_FORMAT_AYUV,
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_Y410, ID3D11Texture2D,
};
use crate::ffi::vpl::{
    MFX_CHROMAFORMAT_YUV420, MFX_CHROMAFORMAT_YUV444, MFX_CODEC_AVC, MFX_CODEC_HEVC,
    MFX_CODINGOPTION_OFF, MFX_CODINGOPTION_ON, MFX_ERR_MORE_DATA, MFX_EXTBUFF_CODING_OPTION,
    MFX_EXTBUFF_CODING_OPTION2, MFX_EXTBUFF_CODING_OPTION3, MFX_EXTBUFF_ENCODER_RESET_OPTION,
    MFX_EXTBUFF_VIDEO_SIGNAL_INFO, MFX_FOURCC_AYUV, MFX_FOURCC_NV12, MFX_FOURCC_P010,
    MFX_FOURCC_Y410, MFX_FRAMETYPE_I, MFX_FRAMETYPE_IDR, MFX_GOP_STRICT,
    MFX_IOPATTERN_IN_VIDEO_MEMORY, MFX_PICSTRUCT_PROGRESSIVE, MFX_PROFILE_AVC_HIGH,
    MFX_PROFILE_HEVC_MAIN, MFX_PROFILE_HEVC_MAIN10, MFX_PROFILE_HEVC_REXT, MFX_RATECONTROL_VBR,
    MFX_SCENARIO_REMOTE_GAMING, MFX_TARGETUSAGE_BEST_QUALITY, MFX_TARGETUSAGE_BEST_SPEED,
    mfxBitstream, mfxExtBuffer, mfxExtCodingOption, mfxExtCodingOption2, mfxExtCodingOption3,
    mfxExtEncoderResetOption, mfxExtVideoSignalInfo, mfxFrameInfo,
    mfxFrameInfo__bindgen_ty_1__bindgen_ty_1, mfxFrameSurface1, mfxVideoParam,
};
use crate::upload::{describe, upload};
use crate::{Encoder as EncoderTrait, Poll};

/// How many conversion targets a frame source rotates through for this
/// encoder, as for NVIDIA's and AMD's: one written while the engine reads
/// another. The engine itself takes one picture at a time.
pub const IN_FLIGHT: usize = 4;

/// The rate's buffer, in pictures of the rate the encoder is built with.
const BUFFER_PICTURES: u64 = 6;

/// What a picture's bitstream holds beyond the samples of the picture
/// uncompressed: the parameter sets and the slices' headers of a picture
/// coded as large as it can be.
const BITSTREAM_MARGIN: usize = 64 * 1024;

/// The most the runtime's sixteen-bit rate fields carry, before the
/// multiplier they share.
const RATE_FIELD: u64 = 0xffff;

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

const ON: u16 = word(MFX_CODINGOPTION_ON);
const OFF: u16 = word(MFX_CODINGOPTION_OFF);
const VIDEO_MEMORY: u16 = word(MFX_IOPATTERN_IN_VIDEO_MEMORY);
const PROGRESSIVE: u16 = word(MFX_PICSTRUCT_PROGRESSIVE);
const GOP_STRICT: u16 = word(MFX_GOP_STRICT);
const VBR: u16 = word(MFX_RATECONTROL_VBR);
const FASTEST: u16 = word(MFX_TARGETUSAGE_BEST_SPEED);
const MOST_THOROUGH: u16 = word(MFX_TARGETUSAGE_BEST_QUALITY);
const REMOTE_GAMING: u16 = word(MFX_SCENARIO_REMOTE_GAMING);
const YUV420: u16 = word(MFX_CHROMAFORMAT_YUV420);
const YUV444: u16 = word(MFX_CHROMAFORMAT_YUV444);
const AVC_HIGH: u16 = word(MFX_PROFILE_AVC_HIGH);
const HEVC_MAIN: u16 = word(MFX_PROFILE_HEVC_MAIN);
const HEVC_MAIN10: u16 = word(MFX_PROFILE_HEVC_MAIN10);
const HEVC_REXT: u16 = word(MFX_PROFILE_HEVC_REXT);
/// The types a keyframe reports: an IDR picture, or an intra one.
const KEYFRAME: u16 = word(MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR);

/// The colour the host states ([05 §3.1](../../../docs/05-host.md)): no
/// analogue format, BT.709 for the primaries, the transfer and the matrix,
/// in the video range.
const UNSPECIFIED_FORMAT: u16 = 5;
const BT709: u16 = 1;

/// Why the encoder could not be built or a call on it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The runtime refused a call: among them a build the part does not
    /// code -- a size, a rate of pictures or a layout -- which it is asked
    /// for, never assumed to.
    Runtime(runtime::Error),
    /// Ten bits or full chroma asked of the first codec, which codes neither
    /// here.
    H264Profile,
    /// The picture's size outside what the runtime's fields carry.
    Size,
    /// A texture in a layout or of a size the encoder was not built to read,
    /// or a frame from ordinary memory for full chroma, which two planes
    /// cannot carry.
    Layout,
    /// An input the encoder was never given.
    Unregistered,
    /// As many pictures in flight as the encoder holds: back pressure.
    QueueFull,
    /// The runtime reported a unit outside the bitstream it was written into.
    Unit,
    /// The device refused a texture or a mapping.
    Device(lowlat_drivers::d3d11::Error),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Runtime(error) => write!(f, "{error}"),
            Self::H264Profile => {
                f.write_str("Intel's encoder has no ten-bit or full-chroma H.264 here")
            }
            Self::Size => f.write_str("the picture is outside the sizes Intel's encoder takes"),
            Self::Layout => f.write_str("a texture Intel's encoder was not built to read"),
            Self::Unregistered => f.write_str("an input Intel's encoder was never given"),
            Self::QueueFull => f.write_str("Intel's encoder holds as many pictures as it takes"),
            Self::Unit => f.write_str("Intel's encoder reported a unit outside its bitstream"),
            Self::Device(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<runtime::Error> for Error {
    fn from(error: runtime::Error) -> Self {
        Self::Runtime(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// How much chroma a session codes: full chroma only past the census of the
/// captured output's encoder, and only from the display's conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Chroma {
    #[default]
    Yuv420,
    Yuv444,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Depth {
    #[default]
    Eight,
    Ten,
}

/// How the encoder is built. Fixed for its life but for the rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// The rate the encoder is built at, which the rate's buffer is made
    /// from: the session's ceiling, since no change can move the buffer.
    pub bitrate_bps: u32,
    /// The quantiser floor, zero for none ([`crate::Quality::min_qp`]).
    pub min_qp: u32,
    /// The most thorough effort the encoder has rather than the fastest
    /// ([`crate::Quality::thorough`]).
    pub thorough: bool,
    pub chroma: Chroma,
    pub depth: Depth,
}

impl Config {
    /// The layout the encoder reads, as the runtime and the device name it.
    const fn layout(self) -> (u32, DXGI_FORMAT) {
        match (self.chroma, self.depth) {
            (Chroma::Yuv420, Depth::Eight) => (MFX_FOURCC_NV12 as u32, DXGI_FORMAT_NV12),
            (Chroma::Yuv420, Depth::Ten) => (MFX_FOURCC_P010 as u32, DXGI_FORMAT_P010),
            (Chroma::Yuv444, Depth::Eight) => (MFX_FOURCC_AYUV as u32, DXGI_FORMAT_AYUV),
            (Chroma::Yuv444, Depth::Ten) => (MFX_FOURCC_Y410 as u32, DXGI_FORMAT_Y410),
        }
    }

    /// The bytes one picture's samples take in that layout: the most a
    /// coded picture comes to.
    fn picture_bytes(self) -> Option<usize> {
        let samples = usize::try_from(self.width)
            .ok()?
            .checked_mul(usize::try_from(self.height).ok()?)?;
        match (self.chroma, self.depth) {
            (Chroma::Yuv420, Depth::Eight) => samples.checked_mul(3).map(|bytes| bytes / 2),
            (Chroma::Yuv420, Depth::Ten) => samples.checked_mul(3),
            (Chroma::Yuv444, _) => samples.checked_mul(4),
        }
    }
}

/// An extension buffer's header naming it `id`, sized as `T`.
fn header<T>(id: i32) -> mfxExtBuffer {
    mfxExtBuffer {
        BufferId: u32::try_from(id).unwrap_or(0),
        BufferSz: u32::try_from(size_of::<T>()).unwrap_or(0),
    }
}

/// A rate in kilobits a second, as the runtime's rate fields count.
fn kilobits(bps: u32) -> u64 {
    u64::from(bps) / 1000
}

/// The parameters the encoder is built with and changed by, and the buffers
/// they name. **A change starts from these, as the build left them, never
/// from what the runtime reports back**: from that the first codec's first
/// change is a keyframe.
struct Params {
    video: mfxVideoParam,
    options: mfxExtCodingOption,
    options2: mfxExtCodingOption2,
    options3: mfxExtCodingOption3,
    signal: mfxExtVideoSignalInfo,
    /// Named for a change alone: named on a build's query it would ask what
    /// a change makes of the parameters instead.
    reset: mfxExtEncoderResetOption,
    buffers: [*mut mfxExtBuffer; 5],
}

impl Params {
    fn new(config: &Config) -> Result<Self, Error> {
        if config.width == 0 || config.height == 0 || config.fps == 0 {
            return Err(Error::Size);
        }
        let fits = |value: u32| u16::try_from(value).map_err(|_| Error::Size);
        let (width, height) = (
            fits(config.width.next_multiple_of(16))?,
            fits(config.height.next_multiple_of(16))?,
        );
        let (crop_width, crop_height) = (fits(config.width)?, fits(config.height)?);
        let ten = config.depth == Depth::Ten;
        let full = config.chroma == Chroma::Yuv444;
        let depth = if ten { 10 } else { 8 };
        // **Six pictures of the rate built with**, in kilobytes, and half of
        // it at the start; the multiplier the four fields share, from the
        // largest of them.
        let buffer = (u64::from(config.bitrate_bps) * BUFFER_PICTURES / u64::from(config.fps))
            .div_ceil(8000)
            .max(1);
        let multiplier = kilobits(config.bitrate_bps)
            .max(buffer)
            .div_ceil(RATE_FIELD)
            .max(1);
        let field = |value: u64| u16::try_from(value / multiplier).unwrap_or(u16::MAX);

        // SAFETY: plain data, every field of which zero leaves unset.
        let mut params: Self = unsafe { core::mem::zeroed() };
        params.video.AsyncDepth = 1;
        params.video.IOPattern = VIDEO_MEMORY;
        // SAFETY: the union's encoder half, every field of it plain data.
        let mfx = unsafe { &mut params.video.__bindgen_anon_1.mfx };
        mfx.CodecId = match config.codec {
            Codec::H264 => MFX_CODEC_AVC as u32,
            Codec::H265 => MFX_CODEC_HEVC as u32,
        };
        mfx.CodecProfile = match (config.codec, full, ten) {
            (Codec::H264, _, _) => AVC_HIGH,
            (Codec::H265, true, _) => HEVC_REXT,
            (Codec::H265, false, true) => HEVC_MAIN10,
            (Codec::H265, false, false) => HEVC_MAIN,
        };
        // The fixed-function engine, which some parts have alone.
        mfx.LowPower = ON;
        mfx.BRCParamMultiplier = u16::try_from(multiplier).unwrap_or(u16::MAX);
        // SAFETY: as above.
        let coding = unsafe { &mut mfx.__bindgen_anon_1.__bindgen_anon_1 };
        coding.TargetUsage = if config.thorough {
            MOST_THOROUGH
        } else {
            FASTEST
        };
        // Recovery is a keyframe asked for: nothing scheduled, nothing
        // reordered, one picture referenced.
        coding.GopPicSize = 0xffff;
        coding.GopRefDist = 1;
        coding.GopOptFlag = GOP_STRICT;
        coding.IdrInterval = 0;
        coding.NumRefFrame = 1;
        coding.NumSlice = 1;
        coding.RateControlMethod = VBR;
        coding.BufferSizeInKB = field(buffer);
        coding.__bindgen_anon_1.InitialDelayInKB = field(buffer / 2).max(1);
        let info = &mut mfx.FrameInfo;
        info.FourCC = config.layout().0;
        info.ChromaFormat = if full { YUV444 } else { YUV420 };
        info.BitDepthLuma = depth;
        info.BitDepthChroma = depth;
        // The two-plane layout holds its ten bits high in each word.
        info.Shift = u16::from(ten && !full);
        info.__bindgen_anon_1.__bindgen_anon_1 = mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
            Width: width,
            Height: height,
            CropX: 0,
            CropY: 0,
            CropW: crop_width,
            CropH: crop_height,
        };
        info.FrameRateExtN = config.fps;
        info.FrameRateExtD = 1;
        info.PicStruct = PROGRESSIVE;
        info.AspectRatioW = 1;
        info.AspectRatioH = 1;

        // **Nothing with a picture but the picture**: no delimiter, no
        // buffering or timing messages, no recovery point, and the parameter
        // sets with each IDR picture alone.
        let options = &mut params.options;
        options.Header = header::<mfxExtCodingOption>(MFX_EXTBUFF_CODING_OPTION);
        options.AUDelimiter = OFF;
        options.NalHrdConformance = OFF;
        options.VuiNalHrdParameters = OFF;
        options.VuiVclHrdParameters = OFF;
        options.PicTimingSEI = OFF;
        options.RecoveryPointSEI = OFF;
        let options2 = &mut params.options2;
        options2.Header = header::<mfxExtCodingOption2>(MFX_EXTBUFF_CODING_OPTION2);
        options2.RepeatPPS = OFF;
        // A low rate taken as asked, never raised to the runtime's least.
        options2.BitrateLimit = OFF;
        // No picture type of the runtime's choosing.
        options2.AdaptiveI = OFF;
        options2.AdaptiveB = OFF;
        if config.min_qp > 0 {
            // HEVC takes no floor under its own, ten (twenty-two at ten
            // bits), and keeps that one: the query corrects it.
            let floor = u8::try_from(config.min_qp).unwrap_or(u8::MAX);
            let top = if ten { 63 } else { 51 };
            options2.MinQPI = floor;
            options2.MinQPP = floor;
            options2.MinQPB = floor;
            options2.MaxQPI = top;
            options2.MaxQPP = top;
            options2.MaxQPB = top;
        }
        let options3 = &mut params.options3;
        options3.Header = header::<mfxExtCodingOption3>(MFX_EXTBUFF_CODING_OPTION3);
        options3.TargetChromaFormatPlus1 = if full { YUV444 + 1 } else { YUV420 + 1 };
        options3.TargetBitDepthLuma = depth;
        options3.TargetBitDepthChroma = depth;
        // **HEVC's remote-gaming tuning**: the same pictures, sooner. The
        // first codec's changes its rate control and sends a picture set
        // again mid-stream. HEVC's predicted pictures stay the runtime's
        // low-delay bidirectional ones, which every part codes -- some hang
        // their engine on plain predicted ones.
        if config.codec == Codec::H265 {
            options3.ScenarioInfo = REMOTE_GAMING;
        }
        let signal = &mut params.signal;
        signal.Header = header::<mfxExtVideoSignalInfo>(MFX_EXTBUFF_VIDEO_SIGNAL_INFO);
        signal.VideoFormat = UNSPECIFIED_FORMAT;
        signal.VideoFullRange = 0;
        signal.ColourDescriptionPresent = 1;
        signal.ColourPrimaries = BT709;
        signal.TransferCharacteristics = BT709;
        signal.MatrixCoefficients = BT709;
        let reset = &mut params.reset;
        reset.Header = header::<mfxExtEncoderResetOption>(MFX_EXTBUFF_ENCODER_RESET_OPTION);
        // **A change that would need a new sequence is refused**, never made
        // with a keyframe.
        reset.StartNewSequence = OFF;
        params.set_rate(config.bitrate_bps, config.fps);
        Ok(params)
    }

    /// The target and the peak at `bitrate_bps`, and the largest picture at
    /// the buffer rule's size for that rate. The buffer stays as built.
    fn set_rate(&mut self, bitrate_bps: u32, fps: u32) {
        // SAFETY: the union's encoder half, every field of it plain data.
        let mfx = unsafe { &mut self.video.__bindgen_anon_1.mfx };
        let multiplier = u64::from(mfx.BRCParamMultiplier.max(1));
        let target = u16::try_from((kilobits(bitrate_bps) / multiplier).max(1)).unwrap_or(u16::MAX);
        // SAFETY: as above.
        let coding = unsafe { &mut mfx.__bindgen_anon_1.__bindgen_anon_1 };
        coding.__bindgen_anon_2.TargetKbps = target;
        coding.__bindgen_anon_3.MaxKbps = target;
        self.options2.MaxFrameSize = crate::nvenc::vbv_bits(bitrate_bps, fps) / 8;
    }

    /// The parameters, naming their buffers afresh for one call: a build's,
    /// or with `change` a change's as well.
    fn named(&mut self, change: bool) -> &mut mfxVideoParam {
        self.buffers = [
            &raw mut self.options.Header,
            &raw mut self.options2.Header,
            &raw mut self.options3.Header,
            &raw mut self.signal.Header,
            &raw mut self.reset.Header,
        ];
        self.video.ExtParam = self.buffers.as_mut_ptr();
        self.video.NumExtParam = if change { 5 } else { 4 };
        &mut self.video
    }

    /// The pictures' description as the runtime left it.
    fn info(&self) -> mfxFrameInfo {
        // SAFETY: the union's encoder half, written by the build.
        unsafe { self.video.__bindgen_anon_1.mfx.FrameInfo }
    }
}

/// Whether the encoder on `session` codes `config`, as the build would ask
/// it -- a size, a rate of pictures or a layout the part does not code is
/// refused -- with nothing built: what a census asks.
pub fn codes(session: &Session<'_>, config: &Config) -> bool {
    if config.codec == Codec::H264
        && (config.depth == Depth::Ten || config.chroma == Chroma::Yuv444)
    {
        return false;
    }
    let Ok(mut params) = Params::new(config) else {
        return false;
    };
    // SAFETY: the buffers the parameters name are their own, live for the
    // call.
    unsafe { session.query_encoder(params.named(false)) }.is_ok()
}

/// A texture the encoder reads and the surface made of it, which the runtime
/// locks and writes while a picture of it is in flight: reached through its
/// pointer alone, and freed only once the encoder is closed.
struct Input {
    surface: NonNull<mfxFrameSurface1>,
    /// Held while the surface names it.
    texture: Com<ID3D11Texture2D>,
}

impl Input {
    fn new(info: mfxFrameInfo, texture: Com<ID3D11Texture2D>) -> Self {
        let surface = runtime::input(info, texture.as_ptr().cast());
        Self {
            surface: NonNull::from(Box::leak(Box::new(surface))),
            texture,
        }
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        // SAFETY: made by `Box::leak` and freed once, here, after the encoder
        // that may have held it was closed.
        unsafe { drop(Box::from_raw(self.surface.as_ptr())) };
    }
}

/// One of the generator's inputs: the texture the encoder reads with its
/// surface, and the one the processor writes it through.
struct Slot {
    input: Input,
    staging: Com<ID3D11Texture2D>,
}

/// The bitstream the picture in the engine is written into, and the memory
/// it writes: made once at the largest a picture can be, and never moved
/// while the encoder lives. **Reached through raw pointers alone, never a
/// reference** while a picture may be writing: the runtime writes both from
/// threads of its own.
struct Output {
    bitstream: NonNull<mfxBitstream>,
    memory: NonNull<[u8]>,
}

impl Output {
    fn new(capacity: usize) -> Result<Self, Error> {
        let length = u32::try_from(capacity).map_err(|_| Error::Size)?;
        let memory = NonNull::from(Box::leak(vec![0u8; capacity].into_boxed_slice()));
        // SAFETY: plain data, every field of which zero leaves unset.
        let mut bitstream: mfxBitstream = unsafe { core::mem::zeroed() };
        bitstream.Data = memory.as_ptr().cast::<u8>();
        bitstream.MaxLength = length;
        Ok(Self {
            bitstream: NonNull::from(Box::leak(Box::new(bitstream))),
            memory,
        })
    }

    /// The unit written and the type the runtime reported for its picture;
    /// `None` for one reported outside the memory.
    ///
    /// # Safety
    ///
    /// The runtime has finished the picture written into the bitstream and
    /// does not write it again while the unit is borrowed.
    unsafe fn unit(&self) -> Option<(&[u8], u16)> {
        // SAFETY: the bitstream, which the caller says nothing writes.
        let bitstream = unsafe { self.bitstream.as_ref() };
        let offset = usize::try_from(bitstream.DataOffset).ok()?;
        let length = usize::try_from(bitstream.DataLength).ok()?;
        if offset.checked_add(length)? > self.memory.len() {
            return None;
        }
        let bytes = self.memory.as_ptr().cast::<u8>().wrapping_add(offset);
        // SAFETY: inside the memory, checked above, which lives as long as
        // the output; the caller says nothing writes it meanwhile.
        Some((
            unsafe { core::slice::from_raw_parts(bytes, length) },
            bitstream.FrameType,
        ))
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        // SAFETY: both were made by `Box::leak` and are freed once, here,
        // after the encoder that wrote them was closed.
        unsafe {
            drop(Box::from_raw(self.bitstream.as_ptr()));
            drop(Box::from_raw(self.memory.as_ptr()));
        }
    }
}

/// The picture in the engine: what its session waits on, and whether a wait
/// already found it done -- a wait that answered done is not asked again.
struct InFlight {
    pending: Pending,
    done: bool,
}

/// Whether the engine is still at a picture, for the frame source on the
/// encoder's device ([`Encoder::engine`]). **The runtime finishes each
/// picture on that device's context**, so a wait elsewhere that holds the
/// device -- the desktop duplication's -- holds the picture up for as long
/// as it runs.
#[derive(Debug, Clone)]
pub struct Engine(Arc<AtomicBool>);

impl Engine {
    /// True from a picture's submit until a collect or a wait finds it
    /// encoded.
    pub fn busy(&self) -> bool {
        // Set and read on the encoder's own thread: no order to keep.
        self.0.load(Ordering::Relaxed)
    }
}

/// An encoder, on the session and the device it borrows.
pub struct Encoder<'s> {
    session: &'s Session<'s>,
    device: &'s Device,
    config: Config,
    params: Params,
    /// The pictures' description, which each surface carries.
    info: mfxFrameInfo,
    /// Submitted and not yet collected.
    in_engine: Option<InFlight>,
    /// Whether that picture is still being encoded, as [`Engine`] says it.
    busy: Arc<AtomicBool>,
    output: Output,
    /// A rate asked for while a picture was in the engine, made before the
    /// next goes in.
    wanted: Option<u32>,
    /// The generator's input, written only with nothing in the engine; none
    /// at full chroma.
    slot: Option<Slot>,
    /// The inputs made of textures something else writes, each at the place
    /// [`Encoder::register_texture`] answered.
    registered: Vec<Input>,
}

impl core::fmt::Debug for Encoder<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("config", &self.config)
            .field("in_flight", &self.in_flight())
            .finish()
    }
}

impl<'s> Encoder<'s> {
    /// Build an encoder for `config` on `session` -- one
    /// [`runtime::Vpl::encoder_session`] made on `device`, the device every
    /// texture it reads is on.
    pub fn new(
        session: &'s Session<'s>,
        device: &'s Device,
        config: Config,
    ) -> Result<Self, Error> {
        if config.codec == Codec::H264
            && (config.depth == Depth::Ten || config.chroma == Chroma::Yuv444)
        {
            return Err(Error::H264Profile);
        }
        let mut params = Params::new(&config)?;
        // **Asked of the runtime, never assumed**: a size, a rate of pictures
        // or a layout the part does not code is refused here, and what it
        // corrects -- the floor, the start of the buffer -- is kept corrected.
        // SAFETY: the buffers the parameters name are their own, live for the
        // call.
        unsafe { session.query_encoder(params.named(false)) }?;
        let info = params.info();
        let capacity = config
            .picture_bytes()
            .and_then(|bytes| bytes.checked_add(BITSTREAM_MARGIN))
            .ok_or(Error::Size)?;
        let output = Output::new(capacity)?;
        let slot = match config.chroma {
            Chroma::Yuv420 => {
                let (_, format) = config.layout();
                let size = (config.width, config.height);
                let texture = device
                    .texture(&describe(format, size, D3D11_USAGE_DEFAULT))
                    .map_err(Error::Device)?;
                let staging = device
                    .texture(&describe(format, size, D3D11_USAGE_STAGING))
                    .map_err(Error::Device)?;
                Some(Slot {
                    input: Input::new(info, texture),
                    staging,
                })
            }
            // Two planes from ordinary memory cannot carry full chroma.
            Chroma::Yuv444 => None,
        };
        // Last of what can fail, so nothing is left built when the rest is
        // refused.
        // SAFETY: as above.
        unsafe { session.init_encoder(params.named(false)) }?;
        Ok(Self {
            session,
            device,
            config,
            params,
            info,
            in_engine: None,
            busy: Arc::new(AtomicBool::new(false)),
            output,
            wanted: None,
            slot,
            registered: Vec::new(),
        })
    }

    pub fn config(&self) -> Config {
        self.config
    }

    /// Pictures submitted and not yet collected: one at most.
    pub fn in_flight(&self) -> usize {
        usize::from(self.in_engine.is_some())
    }

    /// Whether the engine is at a picture, as the frame source sees it.
    pub fn engine(&self) -> Engine {
        Engine(Arc::clone(&self.busy))
    }

    /// `texture`, which something else on the encoder's device writes -- a
    /// conversion's target -- made an input the encoder keeps, answered by
    /// its place among them: submitted by that place with
    /// [`Encoder::submit_registered`] as often as it holds a picture. The
    /// encoder holds the texture as long as it lives.
    pub fn register_texture(&mut self, texture: &Com<ID3D11Texture2D>) -> Result<usize, Error> {
        // SAFETY: plain data the call fills whole.
        let mut desc: D3D11_TEXTURE2D_DESC = unsafe { core::mem::zeroed() };
        // SAFETY: a live texture; the output is a live local.
        unsafe { vcall!(texture.as_ptr(), GetDesc, &raw mut desc) };
        let (_, format) = self.config.layout();
        if desc.Format != format
            || desc.Width < self.config.width
            || desc.Height < self.config.height
        {
            return Err(Error::Layout);
        }
        self.registered
            .push(Input::new(self.info, texture.clone_ref()));
        Ok(self.registered.len() - 1)
    }

    /// Hand over a picture already on the device, in the input
    /// [`Encoder::register_texture`] answered `input` for. The texture must
    /// hold the picture until the unit made from it is collected.
    pub fn submit_registered(&mut self, input: usize, force_keyframe: bool) -> Result<(), Error> {
        let surface = self
            .registered
            .get(input)
            .ok_or(Error::Unregistered)?
            .surface;
        self.queue(surface, force_keyframe)
    }

    /// Hand over a frame from ordinary memory, written into the generator's
    /// input first.
    pub fn submit(
        &mut self,
        frame: &lowlat_capture::Frame<'_>,
        force_keyframe: bool,
    ) -> Result<(), Error> {
        // Checked before the input is written, which the engine may be
        // reading otherwise.
        if self.in_engine.is_some() {
            return Err(Error::QueueFull);
        }
        let slot = self.slot.as_ref().ok_or(Error::Layout)?;
        upload(
            self.device,
            &slot.staging,
            &slot.input.texture,
            frame,
            (self.config.width, self.config.height),
            self.config.depth == Depth::Ten,
        )
        .map_err(Error::Device)?;
        let surface = slot.input.surface;
        self.queue(surface, force_keyframe)
    }

    /// Hand `surface` to the engine for one picture, after a change of rate
    /// asked for while the last was in it. A change refused there is let go:
    /// the picture goes in at the rate before it.
    fn queue(
        &mut self,
        surface: NonNull<mfxFrameSurface1>,
        force_keyframe: bool,
    ) -> Result<(), Error> {
        if self.in_engine.is_some() {
            return Err(Error::QueueFull);
        }
        if let Some(bitrate_bps) = self.wanted.take() {
            self.change_rate(bitrate_bps)?;
        }
        // SAFETY: the surface is one `runtime::input` made of a texture of
        // the session's device, the bitstream the output's over its own
        // memory; both stay in place until the encoder is closed, and neither
        // is touched here while the picture is in the engine.
        let encoded = unsafe {
            self.session.encode(
                surface.as_ptr(),
                self.output.bitstream.as_ptr(),
                force_keyframe,
            )
        }?;
        match encoded {
            Encoded::Pending(pending) => {
                self.in_engine = Some(InFlight {
                    pending,
                    done: false,
                });
                self.busy.store(true, Ordering::Relaxed);
                Ok(())
            }
            Encoded::Busy => Err(Error::QueueFull),
            // Never with one picture of depth and nothing reordered: a
            // picture held back would hand every unit out one late.
            Encoded::MoreData => Err(Error::Runtime(runtime::Error::Status(MFX_ERR_MORE_DATA))),
        }
    }

    /// The picture's unit, if it is encoded; never blocks. The bytes are the
    /// encoder's own, valid until the next call.
    pub fn poll(&mut self) -> Result<Poll<'_>, Error> {
        let Some(in_engine) = self.in_engine.as_mut() else {
            return Ok(Poll::Pending);
        };
        if !in_engine.done {
            in_engine.done = self.session.sync_encoded(&in_engine.pending, 0)?;
            if !in_engine.done {
                return Ok(Poll::Pending);
            }
        }
        self.busy.store(false, Ordering::Relaxed);
        self.in_engine = None;
        // SAFETY: the runtime has finished the picture, and writes the
        // bitstream again only once a picture is handed over again -- by a
        // call on this encoder, which the borrow the unit carries keeps out.
        let (bitstream, kind) = unsafe { self.output.unit() }.ok_or(Error::Unit)?;
        Ok(Poll::Ready {
            bitstream,
            keyframe: kind & KEYFRAME != 0,
        })
    }

    /// Change the rate, live: the target and the peak, and the largest
    /// picture with them -- at once with nothing in the engine, else before
    /// the next picture goes in.
    pub fn reconfigure(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        if self.in_engine.is_some() {
            self.wanted = Some(bitrate_bps);
            return Ok(());
        }
        self.wanted = None;
        self.change_rate(bitrate_bps)
    }

    /// Make a change of rate, from the parameters the encoder was built with.
    fn change_rate(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        self.params.set_rate(bitrate_bps, self.config.fps);
        // SAFETY: the buffers the parameters name are their own, live for the
        // call.
        unsafe { self.session.reset_encoder(self.params.named(true)) }?;
        self.config.bitrate_bps = bitrate_bps;
        Ok(())
    }

    /// Sleep in the runtime's own wait until the picture is encoded or
    /// `limit` passes; true when there was a picture to sleep on. A failed
    /// wait is the collect's to report.
    pub fn wait_finished(&mut self, limit: Duration) -> bool {
        let Some(in_engine) = self.in_engine.as_mut() else {
            return false;
        };
        if !in_engine.done {
            // Under a millisecond rounds up to one, so a wait never becomes a
            // poll.
            let wait_ms = u32::try_from(limit.as_micros().div_ceil(1000))
                .unwrap_or(u32::MAX)
                .max(1);
            in_engine.done = self
                .session
                .sync_encoded(&in_engine.pending, wait_ms)
                .unwrap_or(false);
            if in_engine.done {
                self.busy.store(false, Ordering::Relaxed);
            }
        }
        true
    }
}

impl Drop for Encoder<'_> {
    /// The encoder closed before anything it reads or writes goes: the close
    /// finishes every picture it holds.
    fn drop(&mut self) {
        self.session.close_encoder();
    }
}

impl EncoderTrait for Encoder<'_> {
    type Error = Error;

    fn submit(
        &mut self,
        frame: &lowlat_capture::Frame<'_>,
        force_keyframe: bool,
    ) -> Result<(), Error> {
        Encoder::submit(self, frame, force_keyframe)
    }

    fn poll(&mut self) -> Result<Poll<'_>, Error> {
        Encoder::poll(self)
    }

    fn reconfigure(&mut self, bitrate_bps: u32) -> Result<(), Error> {
        Encoder::reconfigure(self, bitrate_bps)
    }

    fn wait_finished(&mut self, limit: Duration) -> bool {
        Encoder::wait_finished(self, limit)
    }
}
