//! AMD's own video runtime, opened at run time: its decoder and its encoders,
//! on a device the caller made.
//!
//! **The runtime is the display driver's**, in the system directory, and it
//! answers whatever version it is initialised with, so nothing here is decided
//! by a version number: what a caller needs of a decoder it asks the decoder,
//! by the property's name. The factory the runtime hands out lives as long as
//! the process and is never released.
//!
//! **Its interfaces are C++ classes**, called through their tables. Only the
//! methods used are declared, each at the slot the runtime's headers give it,
//! which a test asserts; the rest of each table is padding. The one pair of
//! overloads the compiler lays out the other way round from the headers' C
//! transcription is in the padding.
//!
//! **Everything is on the caller's device**: a context is initialised on it --
//! which turns the device's multithread protection on, since the runtime's own
//! threads use its immediate context too -- and a decoder's surfaces are that
//! device's textures.
//!
//! **A decoder hands a picture out as soon as its unit is submitted**, the
//! decode still running: work queued on the device after the hand-out is
//! ordered behind the decode by the device, and a fence signalled then passes
//! once the picture is decoded. It takes some thirty units before it pushes
//! back, and then it spins in the submit, so a caller keeps its own count of
//! what is unfinished.
//!
//! **An encoder takes a texture of the device as its input**, wrapped once
//! and submitted again as often as the caller likes, and hands each picture
//! out as a unit in host memory. It reads RGB as well as two-plane YUV and
//! converts RGB in the video engine itself. Three of its settings behave in
//! ways a caller must know: the usage is a preset that resets whatever was set
//! before it; how long a query waits for a picture is read only when the
//! encoder is initialised; and a picture's own properties -- a forced type,
//! the delimiter -- stay on its input until written again.

use core::ffi::{CStr, c_void};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;

use lowlat_common::dynlib::Library;

use crate::d3d11::{Com, Device};
use crate::ffi::d3d11::ID3D11Texture2D;

/// A call's result, as the runtime numbers it.
pub type Status = i32;

const OK: Status = 0;
const EOF: Status = 23;
const REPEAT: Status = 24;
const INPUT_FULL: Status = 25;
const RESOLUTION_CHANGED: Status = 26;
const NEED_MORE_INPUT: Status = 44;

/// The runtime's library, installed with the display driver.
const LIBRARY: &CStr = c"amfrt64.dll";
/// The headers' version, 1.5.0: the most this side asks of a runtime.
const HEADER_VERSION: u64 = 1 << 48 | 5 << 32;
/// Host memory, the kind of buffer a unit is handed over in.
const MEMORY_HOST: i32 = 1;
/// The device interface a context asks its device for: the first, which
/// every device made here has.
const DX11_0: i32 = 110;
const VARIANT_BOOL: i32 = 1;
const VARIANT_INT64: i32 = 2;
const VARIANT_SIZE: i32 = 5;
const VARIANT_RATE: i32 = 7;
/// The acceleration a component reports when its engine is the hardware's.
const ACCELERATION_HARDWARE: i32 = 0;
/// Room for the longest name passed, with its terminator.
const NAME_CAPACITY: usize = 64;

/// How many reference pictures a decoder holds back before it hands one out,
/// an `i64`: [`REORDER_LOW_LATENCY`] or the default, which holds as many as
/// the stream references plus one.
pub const REORDER_MODE: &str = "ReorderMode";
/// Every picture handed out as soon as its unit is decoded, in decode order:
/// right for a stream that does not reorder, and for a caller that puts the
/// order back itself.
pub const REORDER_LOW_LATENCY: i64 = 2;
/// The decoder's low-latency mode, a `bool`: the engine kept at a clock that
/// decodes a picture paced in real time as fast as one back to back, where
/// the default lets the clock fall between pictures.
pub const LOW_LATENCY_DECODE: &str = "LowLatencyDecode";

type QueryVersion = unsafe extern "C" fn(*mut u64) -> Status;
type Init = unsafe extern "C" fn(u64, *mut *mut c_void) -> Status;

/// Why the runtime or a call on it failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No runtime, which is the ordinary case without AMD's driver.
    Unavailable,
    /// Loaded, but missing an entry point it must export.
    MissingSymbol,
    /// A call failed, with its status.
    Status(Status),
    /// More than a buffer or a name holds.
    TooLarge,
    /// A setting read back as something other than the number asked for.
    NotANumber,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => f.write_str("AMD video runtime not available"),
            Self::MissingSymbol => f.write_str("AMD video runtime is missing an entry point"),
            Self::Status(s) => write!(f, "AMD video runtime returned status {s}"),
            Self::TooLarge => f.write_str("more than an AMD video buffer holds"),
            Self::NotANumber => f.write_str("an AMD video setting read back as no number"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

fn check(status: Status) -> Result<()> {
    if status == OK {
        Ok(())
    } else {
        Err(Error::Status(status))
    }
}

/// The codecs, each with a decoder and an encoder of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    /// Both depths: a ten-bit stream's decoder is a component of its own,
    /// named by the layout it hands pictures out in, and an encoder is told
    /// its depth.
    Hevc,
}

impl Codec {
    /// The decoder for pictures in `layout`. **Ten-bit HEVC has a component
    /// of its own**, which the headers call superseded: runtime 1.4.31's
    /// other one, given a ten-bit stream, corrupts the process's memory, and
    /// 1.4.37 decodes either way.
    const fn decoder_id(self, layout: Layout) -> &'static str {
        match (self, layout) {
            (Self::H264, _) => "AMFVideoDecoderUVD_H264_AVC",
            (Self::Hevc, Layout::Nv12) => "AMFVideoDecoderHW_H265_HEVC",
            (Self::Hevc, Layout::P010) => "AMFVideoDecoderHW_H265_MAIN10",
        }
    }

    const fn encoder_id(self) -> &'static str {
        match self {
            Self::H264 => "AMFVideoEncoderVCE_AVC",
            Self::Hevc => "AMFVideoEncoderHW_HEVC",
        }
    }

    /// What this codec's encoder calls its settings, and the values some of
    /// them take.
    pub const fn names(self) -> &'static Names {
        match self {
            Self::H264 => &H264_NAMES,
            Self::Hevc => &HEVC_NAMES,
        }
    }
}

/// What an encoder calls a setting, for one codec: the two components name
/// the same settings differently and number some of their values
/// differently, so a caller writes each setting once and takes its name and
/// value from here.
#[derive(Debug)]
pub struct Names {
    /// The preset every other setting is written after.
    pub usage: &'static str,
    pub profile: &'static str,
    pub frame_size: &'static str,
    pub frame_rate: &'static str,
    pub quality_preset: &'static str,
    pub rate_control: &'static str,
    pub target_bitrate: &'static str,
    pub peak_bitrate: &'static str,
    /// The rate's buffer, in bits.
    pub vbv_buffer_size: &'static str,
    /// The buffer's fullness at the start, 0 to [`VBV_FULL`].
    pub vbv_initial_fullness: &'static str,
    /// The largest picture, in bits.
    pub max_au_size: &'static str,
    pub filler: &'static str,
    pub skip_frame: &'static str,
    pub max_reference_frames: &'static str,
    pub slices: &'static str,
    /// The quantiser floor: one setting on H.264, one per picture kind on
    /// HEVC.
    pub min_qp: &'static [&'static str],
    /// How long a query waits for a picture, in milliseconds; read only when
    /// the encoder is initialised.
    pub query_timeout: &'static str,
    /// A pass ahead of the encode for the rate's sake: an integer on H.264,
    /// a flag on HEVC.
    pub preencode: &'static str,
    /// The full-range flag of the output, which both take as a flag.
    pub full_range: &'static str,
    pub in_color_profile: &'static str,
    pub out_color_profile: &'static str,
    pub in_primaries: &'static str,
    pub out_primaries: &'static str,
    pub in_transfer: &'static str,
    pub out_transfer: &'static str,
    pub color_bit_depth: &'static str,
    /// A rolling intra refresh's size per slot; zero is none.
    pub intra_refresh: &'static str,
    /// The settings that schedule keyframes; zero in each schedules none.
    pub keyframe_period: &'static [&'static str],
    /// Where the encoder puts the parameter sets by itself, where it can be
    /// told.
    pub header_insertion: Option<&'static str>,
    /// A picture's own: its forced type.
    pub force_picture_type: &'static str,
    /// A picture's own: whether an access-unit delimiter leads it.
    pub insert_aud: &'static str,
    /// A picture's own: the parameter sets put ahead of it.
    pub insert_headers: &'static [&'static str],
    /// A unit's own: the kind of picture it carries.
    pub output_data_type: &'static str,
    /// A capability: whether a query can wait for a picture.
    pub cap_query_timeout: &'static str,
    /// The profile for eight bits, and for ten where the codec has one.
    pub profile_8: i64,
    pub profile_10: Option<i64>,
    /// The rate control that bounds each picture by the buffer.
    pub rate_latency_constrained: i64,
    pub preset_speed: i64,
    pub preset_quality: i64,
}

/// The ultra-low-latency usage, the same number on both codecs.
pub const USAGE_ULTRA_LOW_LATENCY: i64 = 1;
/// A forced picture type: none, and an IDR picture.
pub const PICTURE_NONE: i64 = 0;
pub const PICTURE_IDR: i64 = 2;
/// The kinds a unit reports: an IDR picture and an intra one are keyframes.
pub const OUTPUT_IDR: i64 = 0;
pub const OUTPUT_I: i64 = 1;
/// BT.709, as the colour profile (studio range), the primaries and the
/// transfer each number it.
pub const BT709: i64 = 1;
/// A full buffer at the start.
pub const VBV_FULL: i64 = 64;
/// HEVC's parameter sets ahead of every IDR picture.
pub const HEADERS_AT_IDR: i64 = 2;

const H264_NAMES: Names = Names {
    usage: "Usage",
    profile: "Profile",
    frame_size: "FrameSize",
    frame_rate: "FrameRate",
    quality_preset: "QualityPreset",
    rate_control: "RateControlMethod",
    target_bitrate: "TargetBitrate",
    peak_bitrate: "PeakBitrate",
    vbv_buffer_size: "VBVBufferSize",
    vbv_initial_fullness: "InitialVBVBufferFullness",
    max_au_size: "MaxAUSize",
    filler: "FillerDataEnable",
    skip_frame: "RateControlSkipFrameEnable",
    max_reference_frames: "MaxNumRefFrames",
    slices: "SlicesPerFrame",
    min_qp: &["MinQP"],
    query_timeout: "QueryTimeout",
    preencode: "RateControlPreanalysisEnable",
    full_range: "FullRangeColor",
    in_color_profile: "InColorProfile",
    out_color_profile: "OutColorProfile",
    in_primaries: "InColorPrimaries",
    out_primaries: "OutColorPrimaries",
    in_transfer: "InColorTransferChar",
    out_transfer: "OutColorTransferChar",
    color_bit_depth: "ColorBitDepth",
    intra_refresh: "IntraRefreshMBsNumberPerSlot",
    keyframe_period: &["IDRPeriod"],
    header_insertion: None,
    force_picture_type: "ForcePictureType",
    insert_aud: "InsertAUD",
    insert_headers: &["InsertSPS", "InsertPPS"],
    output_data_type: "OutputDataType",
    cap_query_timeout: "QueryTimeoutSupport",
    profile_8: 100,
    profile_10: None,
    rate_latency_constrained: 3,
    preset_speed: 1,
    preset_quality: 2,
};

const HEVC_NAMES: Names = Names {
    usage: "HevcUsage",
    profile: "HevcProfile",
    frame_size: "HevcFrameSize",
    frame_rate: "HevcFrameRate",
    quality_preset: "HevcQualityPreset",
    rate_control: "HevcRateControlMethod",
    target_bitrate: "HevcTargetBitrate",
    peak_bitrate: "HevcPeakBitrate",
    vbv_buffer_size: "HevcVBVBufferSize",
    vbv_initial_fullness: "HevcInitialVBVBufferFullness",
    max_au_size: "HevcMaxAUSize",
    filler: "HevcFillerDataEnable",
    skip_frame: "HevcRateControlSkipFrameEnable",
    max_reference_frames: "HevcMaxNumRefFrames",
    slices: "HevcSlicesPerFrame",
    min_qp: &["HevcMinQP_I", "HevcMinQP_P"],
    query_timeout: "HevcQueryTimeout",
    preencode: "HevcRateControlPreAnalysisEnable",
    full_range: "HevcNominalRange",
    in_color_profile: "HevcInColorProfile",
    out_color_profile: "HevcOutColorProfile",
    in_primaries: "HevcInColorPrimaries",
    out_primaries: "HevcOutColorPrimaries",
    in_transfer: "HevcInColorTransferChar",
    out_transfer: "HevcOutColorTransferChar",
    color_bit_depth: "HevcColorBitDepth",
    intra_refresh: "HevcIntraRefreshCTBsNumberPerSlot",
    keyframe_period: &["HevcGOPSize", "HevcGOPSPerIDR"],
    header_insertion: Some("HevcHeaderInsertionMode"),
    force_picture_type: "HevcForcePictureType",
    insert_aud: "HevcInsertAUD",
    insert_headers: &["HevcInsertHeader"],
    output_data_type: "HevcOutputDataType",
    cap_query_timeout: "HevcQueryTimeoutSupport",
    profile_8: 1,
    profile_10: Some(2),
    rate_latency_constrained: 1,
    preset_speed: 10,
    preset_quality: 0,
};

/// What an encoder is initialised to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Eight bits, a luma plane and an interleaved chroma plane.
    Nv12,
    /// Ten bits in the high bits of sixteen, the same two planes.
    P010,
    /// Eight-bit RGB, converted by the encoder itself.
    Bgra,
    Rgba,
    /// Ten-bit RGB, converted by the encoder itself.
    Rgb10A2,
}

impl Format {
    const fn code(self) -> i32 {
        match self {
            Self::Nv12 => 1,
            Self::Bgra => 3,
            Self::Rgba => 5,
            Self::P010 => 10,
            Self::Rgb10A2 => 13,
        }
    }
}

/// The layouts a decoder hands pictures out in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// Eight bits, a luma plane and an interleaved chroma plane.
    Nv12,
    /// Ten bits in the high bits of sixteen, the same two planes. **A
    /// ten-bit stream must be decoded into this**: into the other it
    /// decodes every picture wrongly, without an error.
    P010,
}

impl Layout {
    const fn code(self) -> i32 {
        match self {
            Self::Nv12 => 1,
            Self::P010 => 10,
        }
    }

    const fn of(code: i32) -> Option<Self> {
        match code {
            1 => Some(Self::Nv12),
            10 => Some(Self::P010),
            _ => None,
        }
    }
}

/// What a submitted unit came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    /// Taken: a picture it completes is handed out by the next query.
    Taken,
    /// Taken, and it completes no picture on its own.
    MoreInput,
    /// Taken, and it holds more than one picture: submitted again with no
    /// unit to go on.
    Repeat,
    /// Not taken: the decoder's queue is full.
    Full,
    /// Not taken: the stream changed size under the decoder.
    ResolutionChanged,
}

/// A name as the runtime takes one: UTF-16, terminated. Every name passed
/// here is ASCII.
struct Name([u16; NAME_CAPACITY]);

impl Name {
    fn new(text: &str) -> Result<Self> {
        let mut out = [0u16; NAME_CAPACITY];
        // The last place stays the terminator.
        let room = out.get_mut(..NAME_CAPACITY - 1).ok_or(Error::TooLarge)?;
        if text.len() > room.len() || !text.is_ascii() {
            return Err(Error::TooLarge);
        }
        for (to, from) in room.iter_mut().zip(text.bytes()) {
            *to = u16::from(from);
        }
        Ok(Self(out))
    }

    fn as_ptr(&self) -> *const u16 {
        self.0.as_ptr()
    }
}

/// An interface's identity.
#[repr(C)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const IID_SURFACE: Guid = Guid {
    data1: 0x3075_dbe3,
    data2: 0x8718,
    data3: 0x4cfa,
    data4: [0x86, 0xfb, 0x21, 0x14, 0xc0, 0xa5, 0xa4, 0x51],
};

/// A property's value: its kind, then a value as wide as the widest the
/// runtime has (a rectangle). Passed by value.
#[repr(C)]
#[derive(Clone, Copy)]
struct Variant {
    kind: i32,
    value: Value,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Value {
    boolean: u8,
    int64: i64,
    /// A rate's numerator and denominator.
    rate: [u32; 2],
    /// A size's width and height.
    size: [i32; 2],
    wide: [u64; 2],
}

impl Variant {
    /// A value for a read to fill: no kind yet.
    fn empty() -> Self {
        Self {
            kind: 0,
            value: Value { wide: [0; 2] },
        }
    }

    fn rate(num: u32, den: u32) -> Self {
        let mut v = Value { wide: [0; 2] };
        v.rate = [num, den];
        Self {
            kind: VARIANT_RATE,
            value: v,
        }
    }

    fn size(width: i32, height: i32) -> Self {
        let mut v = Value { wide: [0; 2] };
        v.size = [width, height];
        Self {
            kind: VARIANT_SIZE,
            value: v,
        }
    }

    /// The integer a read gave, a flag read as zero or one; `None` for a
    /// value of another kind, which a read of an integer setting never
    /// gives.
    fn integer(&self) -> Option<i64> {
        match self.kind {
            // SAFETY: the member the kind names.
            VARIANT_INT64 => Some(unsafe { self.value.int64 }),
            // SAFETY: as above.
            VARIANT_BOOL => Some(i64::from(unsafe { self.value.boolean })),
            _ => None,
        }
    }

    fn boolean(value: bool) -> Self {
        let mut v = Value { wide: [0; 2] };
        v.boolean = u8::from(value);
        Self {
            kind: VARIANT_BOOL,
            value: v,
        }
    }

    fn int64(value: i64) -> Self {
        let mut v = Value { wide: [0; 2] };
        v.int64 = value;
        Self {
            kind: VARIANT_INT64,
            value: v,
        }
    }
}

// The tables. Every method takes the object as its first argument; a slot
// not used is a `usize` of padding, so each used one sits at its place.

/// The base interface's three, which begin every table but the factory's.
#[repr(C)]
struct BaseTable {
    acquire: unsafe extern "system" fn(*mut c_void) -> i32,
    release: unsafe extern "system" fn(*mut c_void) -> i32,
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> Status,
}

#[repr(C)]
struct FactoryTable {
    create_context: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status,
    create_component:
        unsafe extern "system" fn(*mut c_void, *mut c_void, *const u16, *mut *mut c_void) -> Status,
    rest: [usize; 5],
}

/// The property storage's ten, which follow the base interface's in every
/// table that stores properties: a component's, a surface's, a buffer's and
/// the capabilities'.
#[repr(C)]
struct StorageTable {
    set_property: unsafe extern "system" fn(*mut c_void, *const u16, Variant) -> Status,
    get_property: unsafe extern "system" fn(*mut c_void, *const u16, *mut Variant) -> Status,
    has_property: unsafe extern "system" fn(*mut c_void, *const u16) -> u8,
    rest: [usize; 7],
}

/// The beginning every property storage's table shares, which is all a read
/// or a write of a property needs.
#[repr(C)]
struct PropertyTable {
    base: BaseTable,
    storage: StorageTable,
}

#[repr(C)]
struct ContextTable {
    base: BaseTable,
    storage: [usize; 10],
    terminate: unsafe extern "system" fn(*mut c_void) -> Status,
    dx9: [usize; 4],
    init_dx11: unsafe extern "system" fn(*mut c_void, *mut c_void, i32) -> Status,
    devices: [usize; 24],
    alloc_buffer: unsafe extern "system" fn(*mut c_void, i32, usize, *mut *mut c_void) -> Status,
    surfaces: [usize; 5],
    create_surface_from_dx11_native: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *mut *mut c_void,
        *mut c_void,
    ) -> Status,
    rest: [usize; 5],
}

#[repr(C)]
struct ComponentTable {
    base: BaseTable,
    set_property: unsafe extern "system" fn(*mut c_void, *const u16, Variant) -> Status,
    get_property: unsafe extern "system" fn(*mut c_void, *const u16, *mut Variant) -> Status,
    has_property: unsafe extern "system" fn(*mut c_void, *const u16) -> u8,
    storage: [usize; 11],
    init: unsafe extern "system" fn(*mut c_void, i32, i32, i32) -> Status,
    reinit: usize,
    terminate: unsafe extern "system" fn(*mut c_void) -> Status,
    drain_flush: [usize; 2],
    submit_input: unsafe extern "system" fn(*mut c_void, *mut c_void) -> Status,
    query_output: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status,
    context_allocator: [usize; 2],
    get_caps: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status,
    optimize: usize,
}

/// A buffer's: the data interface's, then its own.
#[repr(C)]
struct BufferTable {
    base: BaseTable,
    storage: StorageTable,
    data: [usize; 6],
    set_pts: unsafe extern "system" fn(*mut c_void, i64),
    get_pts: usize,
    duration: [usize; 2],
    set_size: unsafe extern "system" fn(*mut c_void, usize) -> Status,
    get_size: unsafe extern "system" fn(*mut c_void) -> usize,
    get_native: unsafe extern "system" fn(*mut c_void) -> *mut c_void,
    observers: [usize; 2],
}

/// A component's capabilities: properties, then its own three.
#[repr(C)]
struct CapsTable {
    base: BaseTable,
    storage: StorageTable,
    get_acceleration_type: unsafe extern "system" fn(*mut c_void) -> i32,
    get_input_caps: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> Status,
    get_output_caps: usize,
}

/// What one side of a component takes or gives.
#[repr(C)]
struct IoCapsTable {
    base: BaseTable,
    get_width_range: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32),
    get_height_range: unsafe extern "system" fn(*mut c_void, *mut i32, *mut i32),
    rest: [usize; 6],
}

/// A surface's: the data interface's, then its own.
#[repr(C)]
struct SurfaceTable {
    base: BaseTable,
    storage: StorageTable,
    data: [usize; 6],
    set_pts: usize,
    get_pts: unsafe extern "system" fn(*mut c_void) -> i64,
    duration: [usize; 2],
    get_format: unsafe extern "system" fn(*mut c_void) -> i32,
    planes_count: usize,
    get_plane_at: unsafe extern "system" fn(*mut c_void, usize) -> *mut c_void,
    rest: [usize; 7],
}

#[repr(C)]
struct PlaneTable {
    base: BaseTable,
    kind: usize,
    get_native: unsafe extern "system" fn(*mut c_void) -> *mut c_void,
    rest: [usize; 8],
}

/// The table of the object at `this`.
///
/// # Safety
///
/// `this` is a live object whose table is laid out as `T`, and the table
/// outlives the borrow, which the runtime's tables, being static, do.
unsafe fn table<'t, T>(this: NonNull<c_void>) -> &'t T {
    // SAFETY: the caller's contract; an object begins with its table's
    // address.
    unsafe { &**this.as_ptr().cast::<*const T>() }
}

/// Give up one reference to `this`.
///
/// # Safety
///
/// `this` is a live object this side holds a reference on, given up once.
unsafe fn release(this: NonNull<c_void>) {
    // SAFETY: the caller's contract; every table but the factory's begins
    // with the base interface's.
    let base = unsafe { table::<BaseTable>(this) };
    // SAFETY: as above.
    unsafe { (base.release)(this.as_ptr()) };
}

/// Write the setting `name` of the object at `this`.
///
/// # Safety
///
/// `this` is a live object whose table begins as a property storage's.
unsafe fn write_on(this: NonNull<c_void>, name: &str, value: Variant) -> Result<()> {
    let name = Name::new(name)?;
    // SAFETY: the caller's contract; the name is terminated and the value
    // is plain data passed by value.
    check(unsafe {
        (table::<PropertyTable>(this).storage.set_property)(this.as_ptr(), name.as_ptr(), value)
    })
}

/// Read the setting `name` of the object at `this`, a number or a flag.
///
/// # Safety
///
/// As [`write_on`]. A setting held as a string or an interface would leave
/// its value for the caller to clear, so only numbers are read here.
unsafe fn read_on(this: NonNull<c_void>, name: &str) -> Result<i64> {
    let name = Name::new(name)?;
    let mut value = Variant::empty();
    // SAFETY: the caller's contract; the output is a live local.
    check(unsafe {
        (table::<PropertyTable>(this).storage.get_property)(
            this.as_ptr(),
            name.as_ptr(),
            &raw mut value,
        )
    })?;
    value.integer().ok_or(Error::NotANumber)
}

/// The loaded runtime and the factory it handed out.
pub struct Amf {
    factory: NonNull<c_void>,
    version: u64,
    /// Last, so it outlives the addresses taken from it.
    _library: Library,
}

impl fmt::Debug for Amf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Amf")
            .field("version", &format_args!("{:#x}", self.version))
            .finish()
    }
}

impl Amf {
    /// Open the runtime and initialise it at the lesser of its version and
    /// the headers'.
    pub fn load() -> Result<Self> {
        let library = Library::open(LIBRARY).ok_or(Error::Unavailable)?;
        // SAFETY: both signatures are the runtime header's.
        let query: QueryVersion =
            unsafe { library.symbol(c"AMFQueryVersion") }.ok_or(Error::MissingSymbol)?;
        // SAFETY: as above.
        let init: Init = unsafe { library.symbol(c"AMFInit") }.ok_or(Error::MissingSymbol)?;
        let mut version = 0u64;
        // SAFETY: the output is a live local.
        check(unsafe { query(&raw mut version) })?;
        let mut factory: *mut c_void = core::ptr::null_mut();
        // SAFETY: as above.
        check(unsafe { init(version.min(HEADER_VERSION), &raw mut factory) })?;
        let factory = NonNull::new(factory).ok_or(Error::Unavailable)?;
        Ok(Self {
            factory,
            version,
            _library: library,
        })
    }

    /// The runtime's own version, as it reports it: major, minor, release
    /// and build, sixteen bits each from the top.
    pub fn version(&self) -> u64 {
        self.version
    }

    fn factory(&self) -> &FactoryTable {
        // SAFETY: the factory the runtime handed out, whose table is the
        // factory's and which lives as long as the process.
        unsafe { table(self.factory) }
    }

    /// A context on `device`, which the context must not outlive.
    pub fn context<'d>(&self, device: &'d Device) -> Result<Context<'d>> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live factory; the output is a live local.
        check(unsafe { (self.factory().create_context)(self.factory.as_ptr(), &raw mut raw) })?;
        let context = Context {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            device,
        };
        // SAFETY: a live context and a live device, which outlives it by the
        // lifetime above.
        check(unsafe {
            (context.table().init_dx11)(context.raw.as_ptr(), device.device().cast(), DX11_0)
        })?;
        Ok(context)
    }

    /// A decoder for `codec` in `context`, handing pictures out in `layout`
    /// once initialised in it, not yet initialised, which the context
    /// outlives.
    pub fn decoder<'c>(
        &self,
        context: &'c Context<'_>,
        codec: Codec,
        layout: Layout,
    ) -> Result<Component<'c>> {
        let id = Name::new(codec.decoder_id(layout))?;
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live factory and context; the name and the output are
        // live for the call.
        check(unsafe {
            (self.factory().create_component)(
                self.factory.as_ptr(),
                context.raw.as_ptr(),
                id.as_ptr(),
                &raw mut raw,
            )
        })?;
        Ok(Component {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            _context: PhantomData,
        })
    }

    /// An encoder for `codec` in `context`, not yet initialised, which the
    /// context outlives.
    pub fn encoder<'c>(&self, context: &'c Context<'_>, codec: Codec) -> Result<Encoder<'c>> {
        let id = Name::new(codec.encoder_id())?;
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live factory and context; the name and the output are
        // live for the call.
        check(unsafe {
            (self.factory().create_component)(
                self.factory.as_ptr(),
                context.raw.as_ptr(),
                id.as_ptr(),
                &raw mut raw,
            )
        })?;
        Ok(Encoder {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            codec,
            device: context.device(),
        })
    }
}

/// A context on a device of the caller's: terminated and released on drop,
/// after everything made in it, which borrows it.
pub struct Context<'d> {
    raw: NonNull<c_void>,
    device: &'d Device,
}

impl fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Context")
    }
}

impl<'d> Context<'d> {
    fn table(&self) -> &ContextTable {
        // SAFETY: a live context, whose table is the context's.
        unsafe { table(self.raw) }
    }

    /// The device the context was made on.
    pub fn device(&self) -> &'d Device {
        self.device
    }

    /// A buffer of host memory, `capacity` bytes, a unit at a time handed
    /// over in it.
    pub fn buffer(&self, capacity: usize) -> Result<Buffer<'_>> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live context; the output is a live local.
        check(unsafe {
            (self.table().alloc_buffer)(self.raw.as_ptr(), MEMORY_HOST, capacity, &raw mut raw)
        })?;
        Ok(Buffer {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            capacity,
            _context: PhantomData,
        })
    }

    /// `texture`, which must be the context's device's, wrapped for an
    /// encoder to read: made once and submitted as often as the caller
    /// likes, each time read as the texture is then. The wrapper holds a
    /// reference of its own on the texture.
    pub fn input(&self, texture: &Com<ID3D11Texture2D>) -> Result<Input<'_>> {
        let texture = texture.clone_ref();
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live context and a live texture, which the wrapper keeps
        // alive by its own reference; no observer; the output is a live
        // local.
        check(unsafe {
            (self.table().create_surface_from_dx11_native)(
                self.raw.as_ptr(),
                texture.as_ptr().cast(),
                &raw mut raw,
                core::ptr::null_mut(),
            )
        })?;
        Ok(Input {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            _texture: texture,
            _context: PhantomData,
        })
    }
}

impl Drop for Context<'_> {
    fn drop(&mut self) {
        // SAFETY: a live context, terminated once and then released once.
        unsafe {
            (self.table().terminate)(self.raw.as_ptr());
            release(self.raw);
        }
    }
}

/// A decoder in the context it borrows: terminated and released on drop.
pub struct Component<'c> {
    raw: NonNull<c_void>,
    _context: PhantomData<&'c ()>,
}

impl fmt::Debug for Component<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Component")
    }
}

impl<'c> Component<'c> {
    fn table(&self) -> &ComponentTable {
        // SAFETY: a live component, whose table is the component's.
        unsafe { table(self.raw) }
    }

    /// Whether the decoder has the property `name`: how a runtime is asked
    /// what it can do.
    pub fn has(&self, name: &str) -> bool {
        let Ok(name) = Name::new(name) else {
            return false;
        };
        // SAFETY: a live component and a terminated name.
        unsafe { (self.table().has_property)(self.raw.as_ptr(), name.as_ptr()) != 0 }
    }

    pub fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        self.set(name, Variant::boolean(value))
    }

    pub fn set_int(&self, name: &str, value: i64) -> Result<()> {
        self.set(name, Variant::int64(value))
    }

    fn set(&self, name: &str, value: Variant) -> Result<()> {
        let name = Name::new(name)?;
        // SAFETY: a live component and a terminated name; the value is
        // plain data passed by value.
        check(unsafe { (self.table().set_property)(self.raw.as_ptr(), name.as_ptr(), value) })
    }

    /// Initialise the decoder for pictures of `width` x `height`, handed out
    /// in `layout`.
    pub fn init(&self, layout: Layout, width: u32, height: u32) -> Result<()> {
        let width = i32::try_from(width).map_err(|_| Error::TooLarge)?;
        let height = i32::try_from(height).map_err(|_| Error::TooLarge)?;
        // SAFETY: a live component.
        check(unsafe { (self.table().init)(self.raw.as_ptr(), layout.code(), width, height) })
    }

    /// Hand the decoder the unit in `buffer`. The buffer is the caller's
    /// again once this returns: the decoder keeps no reference to it.
    pub fn submit(&self, buffer: &Buffer) -> Result<Submitted> {
        // SAFETY: a live component and a live buffer.
        Self::submitted(unsafe {
            (self.table().submit_input)(self.raw.as_ptr(), buffer.raw.as_ptr())
        })
    }

    /// Go on with the unit last submitted, which held more than one
    /// picture.
    pub fn resubmit(&self) -> Result<Submitted> {
        // SAFETY: a live component; no unit is the call's own way of saying
        // "the rest of the last one".
        Self::submitted(unsafe {
            (self.table().submit_input)(self.raw.as_ptr(), core::ptr::null_mut())
        })
    }

    fn submitted(status: Status) -> Result<Submitted> {
        match status {
            OK => Ok(Submitted::Taken),
            NEED_MORE_INPUT => Ok(Submitted::MoreInput),
            REPEAT => Ok(Submitted::Repeat),
            INPUT_FULL => Ok(Submitted::Full),
            RESOLUTION_CHANGED => Ok(Submitted::ResolutionChanged),
            other => Err(Error::Status(other)),
        }
    }

    /// The next picture the decoder hands out, if one is ready: at once for
    /// a unit submitted, its decode still running on the device. It borrows
    /// the context, and is given back before the decoder is dropped.
    pub fn query(&self) -> Result<Option<Surface<'c>>> {
        let mut data: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live component; the output is a live local.
        let status = unsafe { (self.table().query_output)(self.raw.as_ptr(), &raw mut data) };
        match status {
            OK => {}
            REPEAT | EOF => return Ok(None),
            other => return Err(Error::Status(other)),
        }
        let Some(data) = NonNull::new(data) else {
            return Ok(None);
        };
        // The data handed out is a surface; its surface interface is asked
        // for, and the data's own reference given up.
        let mut surface: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live object the call handed a reference on; the identity
        // and the output are live.
        let status = unsafe {
            let base = table::<BaseTable>(data);
            let status = (base.query_interface)(data.as_ptr(), &IID_SURFACE, &raw mut surface);
            release(data);
            status
        };
        check(status)?;
        Ok(Some(Surface {
            raw: NonNull::new(surface).ok_or(Error::Status(status))?,
            _context: PhantomData,
        }))
    }
}

impl Drop for Component<'_> {
    fn drop(&mut self) {
        // SAFETY: a live component, terminated once and then released once.
        unsafe {
            (self.table().terminate)(self.raw.as_ptr());
            release(self.raw);
        }
    }
}

/// A buffer of host memory a unit is handed over in, in the context it
/// borrows: made once, filled for each unit, released on drop.
pub struct Buffer<'c> {
    raw: NonNull<c_void>,
    capacity: usize,
    _context: PhantomData<&'c ()>,
}

impl fmt::Debug for Buffer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("capacity", &self.capacity)
            .finish()
    }
}

impl Buffer<'_> {
    fn table(&self) -> &BufferTable {
        // SAFETY: a live buffer, whose table is the buffer's.
        unsafe { table(self.raw) }
    }

    /// Put `bytes` in the buffer, as its whole content.
    pub fn fill(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.capacity {
            return Err(Error::TooLarge);
        }
        // SAFETY: a live buffer, sized within the capacity it was made with.
        check(unsafe { (self.table().set_size)(self.raw.as_ptr(), bytes.len()) })?;
        // SAFETY: as above.
        let native = unsafe { (self.table().get_native)(self.raw.as_ptr()) };
        if native.is_null() {
            return Err(Error::Unavailable);
        }
        // SAFETY: the buffer's host memory, at least `bytes.len()` bytes as
        // just sized, which nothing else writes; the two do not overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), native.cast::<u8>(), bytes.len()) };
        Ok(())
    }

    /// Mark the unit with `pts`, which the surface it completes carries out.
    pub fn set_pts(&self, pts: i64) {
        // SAFETY: a live buffer.
        unsafe { (self.table().set_pts)(self.raw.as_ptr(), pts) };
    }
}

impl Drop for Buffer<'_> {
    fn drop(&mut self) {
        // SAFETY: a live buffer this holds one reference on.
        unsafe { release(self.raw) };
    }
}

/// A decoded picture, handed out while its decode may still be running on
/// the device, in the context it borrows: released on drop, which gives it
/// back to the decoder.
pub struct Surface<'c> {
    raw: NonNull<c_void>,
    _context: PhantomData<&'c ()>,
}

impl fmt::Debug for Surface<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Surface")
    }
}

impl Surface<'_> {
    fn table(&self) -> &SurfaceTable {
        // SAFETY: a live surface, whose table is the surface's.
        unsafe { table(self.raw) }
    }

    /// The mark of the unit that completed it.
    pub fn pts(&self) -> i64 {
        // SAFETY: a live surface.
        unsafe { (self.table().get_pts)(self.raw.as_ptr()) }
    }

    /// The layout the picture is in; `None` for one this side does not know.
    pub fn layout(&self) -> Option<Layout> {
        // SAFETY: a live surface.
        Layout::of(unsafe { (self.table().get_format)(self.raw.as_ptr()) })
    }

    /// The texture the picture is in, an `ID3D11Texture2D` of the context's
    /// device, both planes in the one texture: borrowed from the surface, no
    /// reference of its own, and valid while the surface is held. Null where
    /// the surface has none.
    pub fn texture(&self) -> *mut c_void {
        // SAFETY: a live surface; its first plane is borrowed for the call.
        let plane = unsafe { (self.table().get_plane_at)(self.raw.as_ptr(), 0) };
        let Some(plane) = NonNull::new(plane) else {
            return core::ptr::null_mut();
        };
        // SAFETY: a live plane of a live surface, whose table is the plane's.
        unsafe { (table::<PlaneTable>(plane).get_native)(plane.as_ptr()) }
    }
}

impl Drop for Surface<'_> {
    fn drop(&mut self) {
        // SAFETY: a live surface this holds one reference on.
        unsafe { release(self.raw) };
    }
}

const IID_BUFFER: Guid = Guid {
    data1: 0xb04b_7248,
    data2: 0xb6f0,
    data3: 0x4321,
    data4: [0xb6, 0x91, 0xba, 0xa4, 0x74, 0x0f, 0x9f, 0xcb],
};

/// An encoder in the context it borrows: terminated and released on drop.
pub struct Encoder<'c> {
    raw: NonNull<c_void>,
    codec: Codec,
    /// The context's device, asked at the drop whether it is still there.
    device: &'c Device,
}

impl fmt::Debug for Encoder<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Encoder")
            .field("codec", &self.codec)
            .finish()
    }
}

impl<'c> Encoder<'c> {
    fn table(&self) -> &ComponentTable {
        // SAFETY: a live encoder, whose table is the component's.
        unsafe { table(self.raw) }
    }

    /// The codec it encodes, whose [`Codec::names`] name its settings.
    pub fn codec(&self) -> Codec {
        self.codec
    }

    /// Whether the encoder has the setting `name`: how a runtime is asked
    /// what it can do.
    pub fn has(&self, name: &str) -> bool {
        let Ok(name) = Name::new(name) else {
            return false;
        };
        // SAFETY: a live encoder and a terminated name.
        unsafe { (self.table().has_property)(self.raw.as_ptr(), name.as_ptr()) != 0 }
    }

    pub fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        // SAFETY: a live encoder, whose table begins as a property storage's.
        unsafe { write_on(self.raw, name, Variant::boolean(value)) }
    }

    pub fn set_int(&self, name: &str, value: i64) -> Result<()> {
        // SAFETY: as above.
        unsafe { write_on(self.raw, name, Variant::int64(value)) }
    }

    /// A rate, as `num` pictures in `den` seconds.
    pub fn set_rate(&self, name: &str, num: u32, den: u32) -> Result<()> {
        // SAFETY: as above.
        unsafe { write_on(self.raw, name, Variant::rate(num, den)) }
    }

    pub fn set_size(&self, name: &str, width: u32, height: u32) -> Result<()> {
        let width = i32::try_from(width).map_err(|_| Error::TooLarge)?;
        let height = i32::try_from(height).map_err(|_| Error::TooLarge)?;
        // SAFETY: as above.
        unsafe { write_on(self.raw, name, Variant::size(width, height)) }
    }

    /// A setting's value, a number or a flag.
    pub fn get_int(&self, name: &str) -> Result<i64> {
        // SAFETY: as above.
        unsafe { read_on(self.raw, name) }
    }

    /// What the encoder can do, asked before it is initialised.
    pub fn caps(&self) -> Result<Caps<'c>> {
        let mut raw: *mut c_void = core::ptr::null_mut();
        // SAFETY: a live encoder; the output is a live local.
        check(unsafe { (self.table().get_caps)(self.raw.as_ptr(), &raw mut raw) })?;
        Ok(Caps {
            raw: NonNull::new(raw).ok_or(Error::Unavailable)?,
            _context: PhantomData,
        })
    }

    /// Initialise the encoder for pictures of `width` x `height` in
    /// `format`, after every setting that is read only now.
    pub fn init(&self, format: Format, width: u32, height: u32) -> Result<()> {
        let width = i32::try_from(width).map_err(|_| Error::TooLarge)?;
        let height = i32::try_from(height).map_err(|_| Error::TooLarge)?;
        // SAFETY: a live encoder.
        check(unsafe { (self.table().init)(self.raw.as_ptr(), format.code(), width, height) })
    }

    /// Hand `input` over for one picture: true when it was taken, false when
    /// the encoder holds as many as it takes. The encoder holds the input
    /// until the unit it gives for it is collected.
    pub fn submit(&self, input: &Input<'_>) -> Result<bool> {
        // SAFETY: a live encoder and a live input.
        match unsafe { (self.table().submit_input)(self.raw.as_ptr(), input.raw.as_ptr()) } {
            OK => Ok(true),
            INPUT_FULL => Ok(false),
            other => Err(Error::Status(other)),
        }
    }

    /// The next unit, if one is ready: at once, or after waiting as long as
    /// the encoder was told before it was initialised.
    pub fn query(&self) -> Result<Option<Unit<'c>>> {
        // SAFETY: a live encoder.
        Ok(unsafe { query_on(self.raw) }?.map(|raw| Unit {
            raw,
            _context: PhantomData,
        }))
    }

    /// The encoder's query, for a thread of the caller's that collects its
    /// units while this one submits: the runtime takes a query on one thread
    /// while another submits pictures and writes settings.
    ///
    /// # Safety
    ///
    /// The handle is not used after this encoder is dropped, and every unit
    /// it hands out is released before then.
    pub unsafe fn query_handle(&self) -> QueryHandle {
        QueryHandle { raw: self.raw }
    }
}

/// Query the encoder at `this` once: the buffer of the next unit, if one is
/// ready, with a reference the caller holds.
///
/// # Safety
///
/// `this` is a live encoder.
unsafe fn query_on(this: NonNull<c_void>) -> Result<Option<NonNull<c_void>>> {
    let mut data: *mut c_void = core::ptr::null_mut();
    // SAFETY: the caller's contract; the output is a live local.
    let status =
        unsafe { (table::<ComponentTable>(this).query_output)(this.as_ptr(), &raw mut data) };
    match status {
        OK => {}
        REPEAT | EOF => return Ok(None),
        other => return Err(Error::Status(other)),
    }
    let Some(data) = NonNull::new(data) else {
        return Ok(None);
    };
    // The data handed out is a buffer; its buffer interface is asked for, and
    // the data's own reference given up.
    let mut buffer: *mut c_void = core::ptr::null_mut();
    // SAFETY: a live object the call handed a reference on; the identity and
    // the output are live.
    let status = unsafe {
        let base = table::<BaseTable>(data);
        let status = (base.query_interface)(data.as_ptr(), &IID_BUFFER, &raw mut buffer);
        release(data);
        status
    };
    check(status)?;
    NonNull::new(buffer).map(Some).ok_or(Error::Unavailable)
}

/// An encoder's query, held by the thread that collects its units; see
/// [`Encoder::query_handle`].
pub struct QueryHandle {
    raw: NonNull<c_void>,
}

impl fmt::Debug for QueryHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("QueryHandle")
    }
}

// SAFETY: the runtime takes a query on one thread while another submits and
// writes settings; the handle's maker keeps it inside the encoder's life.
unsafe impl Send for QueryHandle {}

impl QueryHandle {
    /// As [`Encoder::query`], from the collecting thread. The unit belongs
    /// to no lifetime the compiler can see: the handle's maker releases it
    /// before the encoder goes.
    pub fn query(&self) -> Result<Option<Unit<'static>>> {
        // SAFETY: the encoder is live by the handle's contract.
        Ok(unsafe { query_on(self.raw) }?.map(|raw| Unit {
            raw,
            _context: PhantomData,
        }))
    }
}

impl Drop for Encoder<'_> {
    fn drop(&mut self) {
        // **Let go unended on a device that is gone.** The runtime's ending
        // copies on the device, and AMD's driver then reads through a null
        // where it should refuse -- a restart of the driver under an encode,
        // measured. What the encoder holds stays held for the process's life.
        if self.device.lost() {
            return;
        }
        // SAFETY: a live encoder, terminated once and then released once.
        unsafe {
            (self.table().terminate)(self.raw.as_ptr());
            release(self.raw);
        }
    }
}

/// A texture wrapped for an encoder, in the context it borrows: released on
/// drop, and its texture with it. **What it carries for one picture -- a
/// forced type, the delimiter -- stays on it until written again**, so a
/// caller writes them with every picture.
pub struct Input<'c> {
    raw: NonNull<c_void>,
    /// Released after the wrapper, which reads it.
    _texture: Com<ID3D11Texture2D>,
    _context: PhantomData<&'c ()>,
}

impl fmt::Debug for Input<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Input")
    }
}

impl Input<'_> {
    pub fn set_bool(&self, name: &str, value: bool) -> Result<()> {
        // SAFETY: a live surface, whose table begins as a property storage's.
        unsafe { write_on(self.raw, name, Variant::boolean(value)) }
    }

    pub fn set_int(&self, name: &str, value: i64) -> Result<()> {
        // SAFETY: as above.
        unsafe { write_on(self.raw, name, Variant::int64(value)) }
    }
}

impl Drop for Input<'_> {
    fn drop(&mut self) {
        // SAFETY: a live surface this holds one reference on.
        unsafe { release(self.raw) };
    }
}

/// A unit an encoder handed out, in host memory: released on drop.
pub struct Unit<'c> {
    raw: NonNull<c_void>,
    _context: PhantomData<&'c ()>,
}

impl fmt::Debug for Unit<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Unit")
            .field("len", &self.bytes().len())
            .finish()
    }
}

impl Unit<'_> {
    fn table(&self) -> &BufferTable {
        // SAFETY: a live buffer, whose table is the buffer's.
        unsafe { table(self.raw) }
    }

    /// The coded access unit, whole, borrowed while the unit is held.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: a live buffer.
        let (size, native) = unsafe {
            (
                (self.table().get_size)(self.raw.as_ptr()),
                (self.table().get_native)(self.raw.as_ptr()),
            )
        };
        if native.is_null() || size == 0 {
            return &[];
        }
        // SAFETY: the buffer's host memory, `size` bytes, which nothing writes
        // while the unit holds the buffer, and which lives as long as it.
        unsafe { core::slice::from_raw_parts(native.cast::<u8>(), size) }
    }

    /// A number the unit carries, such as the kind of picture.
    pub fn get_int(&self, name: &str) -> Result<i64> {
        // SAFETY: a live buffer, whose table begins as a property storage's.
        unsafe { read_on(self.raw, name) }
    }
}

impl Drop for Unit<'_> {
    fn drop(&mut self) {
        // SAFETY: a live buffer this holds one reference on.
        unsafe { release(self.raw) };
    }
}

// SAFETY: a unit is a reference-counted buffer of the runtime's; reading it and
// giving the reference up from another thread than the one that took it is
// what the count is for, and nothing writes the buffer while it is held.
unsafe impl Send for Unit<'_> {}

/// What an encoder can do: released on drop.
pub struct Caps<'c> {
    raw: NonNull<c_void>,
    _context: PhantomData<&'c ()>,
}

impl fmt::Debug for Caps<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Caps")
    }
}

impl Caps<'_> {
    fn table(&self) -> &CapsTable {
        // SAFETY: live capabilities, whose table is the capabilities'.
        unsafe { table(self.raw) }
    }

    /// Whether the encoder's engine is the hardware's.
    pub fn hardware(&self) -> bool {
        // SAFETY: live capabilities.
        unsafe { (self.table().get_acceleration_type)(self.raw.as_ptr()) == ACCELERATION_HARDWARE }
    }

    /// The pictures it takes: the least and the most width, then height.
    pub fn input_size(&self) -> Result<((i32, i32), (i32, i32))> {
        let mut io: *mut c_void = core::ptr::null_mut();
        // SAFETY: live capabilities; the output is a live local.
        check(unsafe { (self.table().get_input_caps)(self.raw.as_ptr(), &raw mut io) })?;
        let io = NonNull::new(io).ok_or(Error::Unavailable)?;
        let (mut w0, mut w1, mut h0, mut h1) = (0, 0, 0, 0);
        // SAFETY: a live object the call handed a reference on, whose table
        // is the input capabilities'; the outputs are live locals; the
        // reference is given up once.
        unsafe {
            let caps = table::<IoCapsTable>(io);
            (caps.get_width_range)(io.as_ptr(), &raw mut w0, &raw mut w1);
            (caps.get_height_range)(io.as_ptr(), &raw mut h0, &raw mut h1);
            release(io);
        }
        Ok(((w0, w1), (h0, h1)))
    }

    /// A capability by its name, a number or a flag.
    pub fn get_int(&self, name: &str) -> Result<i64> {
        // SAFETY: live capabilities, whose table begins as a property
        // storage's.
        unsafe { read_on(self.raw, name) }
    }
}

impl Drop for Caps<'_> {
    fn drop(&mut self) {
        // SAFETY: live capabilities this holds one reference on.
        unsafe { release(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::D3d11;
    use core::mem::{align_of, offset_of, size_of};

    /// Where slot `n` of a table begins.
    fn slot(n: usize) -> usize {
        n * size_of::<usize>()
    }

    /// **Every method used sits at the slot the runtime's headers give it**,
    /// and every table is as long as the headers' -- counted there by hand,
    /// method by method, since a slot off by one calls another method with
    /// these arguments.
    #[test]
    fn every_method_sits_at_its_slot() {
        assert_eq!(offset_of!(BaseTable, release), slot(1));
        assert_eq!(offset_of!(BaseTable, query_interface), slot(2));

        assert_eq!(offset_of!(FactoryTable, create_context), slot(0));
        assert_eq!(offset_of!(FactoryTable, create_component), slot(1));
        assert_eq!(size_of::<FactoryTable>(), slot(7));

        assert_eq!(offset_of!(ContextTable, terminate), slot(13));
        assert_eq!(offset_of!(ContextTable, init_dx11), slot(18));
        assert_eq!(offset_of!(ContextTable, alloc_buffer), slot(43));
        assert_eq!(size_of::<ContextTable>(), slot(55));

        assert_eq!(offset_of!(ComponentTable, set_property), slot(3));
        assert_eq!(offset_of!(ComponentTable, has_property), slot(5));
        assert_eq!(offset_of!(ComponentTable, init), slot(17));
        assert_eq!(offset_of!(ComponentTable, terminate), slot(19));
        assert_eq!(offset_of!(ComponentTable, submit_input), slot(22));
        assert_eq!(offset_of!(ComponentTable, query_output), slot(23));
        assert_eq!(size_of::<ComponentTable>(), slot(28));

        assert_eq!(offset_of!(BufferTable, set_pts), slot(19));
        assert_eq!(offset_of!(BufferTable, set_size), slot(23));
        assert_eq!(offset_of!(BufferTable, get_native), slot(25));
        assert_eq!(size_of::<BufferTable>(), slot(28));

        assert_eq!(offset_of!(SurfaceTable, get_pts), slot(20));
        assert_eq!(offset_of!(SurfaceTable, get_format), slot(23));
        assert_eq!(offset_of!(SurfaceTable, get_plane_at), slot(25));
        assert_eq!(size_of::<SurfaceTable>(), slot(33));

        assert_eq!(offset_of!(PlaneTable, get_native), slot(4));
        assert_eq!(size_of::<PlaneTable>(), slot(13));

        // The encoders' half, counted from the same headers.
        assert_eq!(offset_of!(StorageTable, get_property), slot(1));
        assert_eq!(offset_of!(StorageTable, has_property), slot(2));
        assert_eq!(size_of::<StorageTable>(), slot(10));
        assert_eq!(offset_of!(PropertyTable, storage), slot(3));
        assert_eq!(
            offset_of!(ContextTable, create_surface_from_dx11_native),
            slot(49)
        );
        assert_eq!(offset_of!(ComponentTable, get_property), slot(4));
        assert_eq!(offset_of!(ComponentTable, get_caps), slot(26));
        assert_eq!(offset_of!(BufferTable, storage), slot(3));
        assert_eq!(offset_of!(BufferTable, get_size), slot(24));
        assert_eq!(offset_of!(SurfaceTable, storage), slot(3));
        assert_eq!(offset_of!(CapsTable, storage), slot(3));
        assert_eq!(offset_of!(CapsTable, get_acceleration_type), slot(13));
        assert_eq!(offset_of!(CapsTable, get_input_caps), slot(14));
        assert_eq!(size_of::<CapsTable>(), slot(16));
        assert_eq!(offset_of!(IoCapsTable, get_width_range), slot(3));
        assert_eq!(offset_of!(IoCapsTable, get_height_range), slot(4));
        assert_eq!(size_of::<IoCapsTable>(), slot(11));
    }

    /// A rate and a size go over as two numbers in the value's first eight
    /// bytes, each under its own kind; a read gives a number back only for
    /// the two kinds that are numbers.
    #[test]
    fn a_rate_and_a_size_are_two_numbers_of_their_own_kinds() {
        let rate = Variant::rate(60, 1);
        assert_eq!(rate.kind, VARIANT_RATE);
        // SAFETY: reading back the member just written.
        assert_eq!(unsafe { rate.value.rate }, [60, 1]);
        let size = Variant::size(1920, 1080);
        assert_eq!(size.kind, VARIANT_SIZE);
        // SAFETY: as above.
        assert_eq!(unsafe { size.value.size }, [1920, 1080]);
        assert_eq!(Variant::int64(5).integer(), Some(5));
        assert_eq!(Variant::boolean(true).integer(), Some(1));
        assert_eq!(rate.integer(), None);
        assert_eq!(Variant::empty().integer(), None);
    }

    /// Every name either codec's encoder is called by goes over whole, and
    /// the two tables differ where the components do.
    #[test]
    fn both_encoders_names_go_over_whole() {
        for codec in [Codec::H264, Codec::Hevc] {
            let n = codec.names();
            let mut all = vec![
                n.usage,
                n.profile,
                n.frame_size,
                n.frame_rate,
                n.quality_preset,
                n.rate_control,
                n.target_bitrate,
                n.peak_bitrate,
                n.vbv_buffer_size,
                n.vbv_initial_fullness,
                n.max_au_size,
                n.filler,
                n.skip_frame,
                n.max_reference_frames,
                n.slices,
                n.query_timeout,
                n.preencode,
                n.full_range,
                n.in_color_profile,
                n.out_color_profile,
                n.in_primaries,
                n.out_primaries,
                n.in_transfer,
                n.out_transfer,
                n.color_bit_depth,
                n.intra_refresh,
                n.force_picture_type,
                n.insert_aud,
                n.output_data_type,
                n.cap_query_timeout,
            ];
            all.extend(n.min_qp);
            all.extend(n.keyframe_period);
            all.extend(n.insert_headers);
            all.extend(n.header_insertion);
            for name in all {
                assert!(Name::new(name).is_ok(), "{codec:?}: {name}");
            }
        }
        let (avc, hevc) = (Codec::H264.names(), Codec::Hevc.names());
        assert_eq!((avc.min_qp.len(), hevc.min_qp.len()), (1, 2));
        assert_eq!(
            (avc.header_insertion, hevc.header_insertion.is_some()),
            (None, true)
        );
        assert_eq!((avc.profile_10, hevc.profile_10), (None, Some(2)));
        assert_eq!(
            (avc.rate_latency_constrained, hevc.rate_latency_constrained),
            (3, 1)
        );
        assert_eq!(Codec::H264.encoder_id(), "AMFVideoEncoderVCE_AVC");
        assert_eq!(Codec::Hevc.encoder_id(), "AMFVideoEncoderHW_HEVC");
    }

    /// **Ten-bit HEVC decodes on a component of its own**: given a ten-bit
    /// stream, an older runtime's other one corrupts the process's memory.
    #[test]
    fn ten_bit_hevc_decodes_on_a_component_of_its_own() {
        assert_eq!(
            Codec::H264.decoder_id(Layout::Nv12),
            "AMFVideoDecoderUVD_H264_AVC"
        );
        assert_eq!(
            Codec::Hevc.decoder_id(Layout::Nv12),
            "AMFVideoDecoderHW_H265_HEVC"
        );
        assert_eq!(
            Codec::Hevc.decoder_id(Layout::P010),
            "AMFVideoDecoderHW_H265_MAIN10"
        );
    }

    /// The formats an encoder is initialised with go over as the runtime
    /// numbers them.
    #[test]
    fn an_encoders_format_is_numbered_as_the_runtime_numbers_it() {
        assert_eq!(Format::Nv12.code(), 1);
        assert_eq!(Format::Bgra.code(), 3);
        assert_eq!(Format::Rgba.code(), 5);
        assert_eq!(Format::P010.code(), 10);
        assert_eq!(Format::Rgb10A2.code(), 13);
    }

    /// The type of the first unit in `bytes` led by a start code.
    fn first_unit_type(bytes: &[u8], codec: Codec) -> Option<u8> {
        let at = bytes.windows(3).position(|w| w == [0, 0, 1])?;
        let header = *bytes.get(at + 3)?;
        Some(match codec {
            Codec::H264 => header & 0x1f,
            Codec::Hevc => (header >> 1) & 0x3f,
        })
    }

    /// On a machine with AMD's runtime and GPU: both encoders build on a
    /// device of ours, report hardware, sizes to 4096x2160 and a query that
    /// can wait, and each turns one texture of ours, forced, into a keyframe
    /// whose first unit is a parameter set with no delimiter ahead of it.
    #[test]
    #[ignore = "requires AMD's runtime and GPU"]
    fn the_runtime_builds_both_encoders_and_each_codes_a_keyframe() {
        use crate::ffi::d3d11::{
            D3D11_BIND_UNORDERED_ACCESS, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
            DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
        };
        let d3d11 = D3d11::load().unwrap();
        let adapter = d3d11
            .adapters()
            .unwrap()
            .into_iter()
            .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
            .expect("an AMD GPU");
        let device = d3d11.open(adapter.luid).unwrap();
        let amf = Amf::load().unwrap();
        let context = amf.context(&device).unwrap();
        let texture = device
            .texture(&D3D11_TEXTURE2D_DESC {
                Width: 1280,
                Height: 720,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: u32::try_from(D3D11_BIND_UNORDERED_ACCESS).unwrap(),
                CPUAccessFlags: 0,
                MiscFlags: 0,
            })
            .unwrap();
        for codec in [Codec::H264, Codec::Hevc] {
            let n = codec.names();
            let encoder = amf.encoder(&context, codec).unwrap();
            let caps = encoder.caps().unwrap();
            assert!(caps.hardware(), "{codec:?}");
            let ((_, width), (_, height)) = caps.input_size().unwrap();
            assert!(
                width >= 4096 && height >= 2160,
                "{codec:?}: {width}x{height}"
            );
            assert_eq!(caps.get_int(n.cap_query_timeout).unwrap(), 1, "{codec:?}");
            drop(caps);
            encoder.set_int(n.usage, USAGE_ULTRA_LOW_LATENCY).unwrap();
            encoder.set_size(n.frame_size, 1280, 720).unwrap();
            encoder.set_rate(n.frame_rate, 60, 1).unwrap();
            encoder.set_int(n.query_timeout, 1000).unwrap();
            if let Some(mode) = n.header_insertion {
                encoder.set_int(mode, HEADERS_AT_IDR).unwrap();
            }
            encoder.init(Format::Nv12, 1280, 720).unwrap();
            let input = context.input(&texture).unwrap();
            input.set_int(n.force_picture_type, PICTURE_IDR).unwrap();
            input.set_bool(n.insert_aud, false).unwrap();
            for header in n.insert_headers {
                input.set_bool(header, true).unwrap();
            }
            assert!(encoder.submit(&input).unwrap(), "{codec:?}: not taken");
            let unit = encoder
                .query()
                .unwrap()
                .expect("a unit within the query's wait");
            assert_eq!(unit.get_int(n.output_data_type).unwrap(), OUTPUT_IDR);
            let parameter_set = match codec {
                Codec::H264 => 7,
                Codec::Hevc => 32,
            };
            assert_eq!(
                first_unit_type(unit.bytes(), codec),
                Some(parameter_set),
                "{codec:?}: {} bytes",
                unit.bytes().len()
            );
        }
    }

    /// A property's value is its kind and then sixteen bytes, eight-aligned:
    /// twenty-four in all, passed by value.
    #[test]
    fn a_variant_is_a_kind_and_sixteen_bytes() {
        assert_eq!(size_of::<Variant>(), 24);
        assert_eq!(align_of::<Variant>(), 8);
        assert_eq!(offset_of!(Variant, value), 8);
        assert_eq!(size_of::<Guid>(), 16);
        // SAFETY: reading back the member just written.
        assert_eq!(unsafe { Variant::int64(2).value.int64 }, 2);
        // SAFETY: as above.
        assert_eq!(unsafe { Variant::boolean(true).value.boolean }, 1);
    }

    /// A name goes over as terminated UTF-16; one too long for the room, or
    /// not ASCII, is refused rather than cut short.
    #[test]
    fn a_name_is_terminated_and_never_cut_short() {
        let name = Name::new(LOW_LATENCY_DECODE).unwrap();
        let text: Vec<u16> = LOW_LATENCY_DECODE.bytes().map(u16::from).collect();
        assert_eq!(&name.0[..text.len()], text.as_slice());
        assert_eq!(name.0[text.len()], 0);
        assert!(Name::new(&"x".repeat(NAME_CAPACITY - 1)).is_ok());
        assert_eq!(
            Name::new(&"x".repeat(NAME_CAPACITY)).err(),
            Some(Error::TooLarge)
        );
        assert_eq!(Name::new("Reorder\u{e9}").err(), Some(Error::TooLarge));
    }

    /// The layouts go over as the runtime numbers them.
    #[test]
    fn a_layout_is_numbered_as_the_runtime_numbers_it() {
        for layout in [Layout::Nv12, Layout::P010] {
            assert_eq!(Layout::of(layout.code()), Some(layout));
        }
        assert_eq!(Layout::Nv12.code(), 1);
        assert_eq!(Layout::P010.code(), 10);
        assert_eq!(Layout::of(15), None);
    }

    /// On a machine with AMD's runtime and GPU: the runtime loads, a
    /// context is made on a device of ours, and both decoders build and
    /// take the two low-latency properties.
    #[test]
    #[ignore = "requires AMD's runtime and GPU"]
    fn the_runtime_builds_both_decoders_on_a_device_of_ours() {
        let d3d11 = D3d11::load().unwrap();
        let adapter = d3d11
            .adapters()
            .unwrap()
            .into_iter()
            .find(|a| a.decodes_here() && a.maker() == Some("AMD"))
            .expect("an AMD GPU");
        let device = d3d11.open(adapter.luid).unwrap();
        let amf = Amf::load().unwrap();
        eprintln!("runtime version {:#x}", amf.version());
        let context = amf.context(&device).unwrap();
        for (codec, layout) in [
            (Codec::H264, Layout::Nv12),
            (Codec::Hevc, Layout::Nv12),
            (Codec::Hevc, Layout::P010),
        ] {
            let decoder = amf.decoder(&context, codec, layout).unwrap();
            assert!(decoder.has(LOW_LATENCY_DECODE), "{codec:?}");
            decoder.set_int(REORDER_MODE, REORDER_LOW_LATENCY).unwrap();
            decoder.set_bool(LOW_LATENCY_DECODE, true).unwrap();
            decoder.init(layout, 1920, 1080).unwrap();
        }
        let mut buffer = context.buffer(4096).unwrap();
        buffer.fill(&[0, 0, 0, 1]).unwrap();
        assert_eq!(buffer.fill(&[0; 4097]).err(), Some(Error::TooLarge));
    }
}
