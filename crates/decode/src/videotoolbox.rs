//! The system's own decoder on macOS: VideoToolbox, through the hardware where
//! the Mac has it and the system's software decoder where it does not, for
//! H.264 and HEVC, ten-bit and full chroma included.
//!
//! **Every unit is read by this crate's reader first**, as the device
//! backends' are: the refusals, the visible picture, the range and the order
//! pictures leave in are the reader's. The decoder is fed each picture's
//! slices in decode order, one call a picture, waited for, and gives every
//! picture back in the order it decoded them whatever it is asked -- the
//! samples carry no presentation time to reorder by -- so a picture is kept
//! under the reader's slot until the reader lets it out. A stream that never
//! reorders, every host's, leaves at once whether or not it says so; one that
//! does leaves in display order.
//!
//! **The session outlives a rebuild that changes nothing.** A host repeats its
//! parameter sets on every keyframe, and each repeat rebuilds the decoder
//! above; sets byte for byte the ones the session was described with keep it,
//! since making one costs milliseconds, and a process's first many more. A
//! set that changes makes a new description, which the session takes where it
//! can and a new session takes otherwise; a fault ends the session.
//!
//! **A picture comes back in the decoder's own buffer**, in one of the
//! two-plane layouts the session is asked for -- chroma interleaved at half
//! size or full, eight bits, or ten in the high bits of sixteen -- and is
//! handed out from there: copied into the caller's planes by
//! [`Decoder::take`], or lent whole by [`Backend::take_buffer`]. Its layout,
//! size and range are checked against what the reader says of the stream, and
//! a picture that differs is refused rather than shown.
//!
//! **The decoder answers on a thread of its own**, even for a call that
//! waits, so the picture is handed over through a slot that carries its own
//! ordering.
//!
//! Nothing of this side's is allocated per unit: the slices are copied
//! length-prefixed into a buffer made once, and the sets live in fixed slots.
//! The two objects a unit the framework takes, a block buffer and a sample
//! buffer, are its own to allocate.

use core::ffi::c_void;
use core::fmt;
use core::ptr::NonNull;

use lowlat_common::clock::{Time, diff_ms};
use lowlat_common::handoff::Handoff;
use lowlat_core::video::{Codec, VideoHeader};
use lowlat_drivers::ffi::vt::{
    CFBooleanRef, CFNumberType, CMBlockBufferRef, CMFormatDescriptionRef, CMSampleBufferRef,
    CMTime, CVImageBufferRef, OSStatus, OSType, OpaqueVTDecompressionSession, VTDecodeInfoFlags,
    VTDecompressionOutputCallbackRecord, VTDecompressionSessionRef, kCFNumberSInt32Type,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
    kCVPixelFormatType_444YpCbCr10BiPlanarFullRange,
    kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange, kVTDecodeInfo_FrameDropped,
};
use lowlat_drivers::vt::{Locked, Object, PixelBuffer, Vt};

use crate::bits::BitReader;
use crate::h264::dpb::Structure;
use crate::nal::Units;
use crate::{Caps, Decoder, Fault, Fed, Format, Picture, Planes, h264, hevc};

/// The layouts a session may hand out, each in both ranges, with what this
/// crate names them: two planes, the chroma interleaved at half size or full,
/// eight bits or ten. Named, since left open the decoder gives ten-bit back
/// in a packed layout no plane describes.
const LAYOUTS: [(OSType, Format, bool); 8] = [
    (
        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
        Format::Nv12,
        false,
    ),
    (
        kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
        Format::Nv12,
        true,
    ),
    (
        kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
        Format::P010,
        false,
    ),
    (
        kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
        Format::P010,
        true,
    ),
    (
        kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange,
        Format::Nv24,
        false,
    ),
    (
        kCVPixelFormatType_444YpCbCr8BiPlanarFullRange,
        Format::Nv24,
        true,
    ),
    (
        kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange,
        Format::P410,
        false,
    ),
    (
        kCVPixelFormatType_444YpCbCr10BiPlanarFullRange,
        Format::P410,
        true,
    ),
];

/// Slots of the readers' picture buffers, each of which may hold a picture
/// decoded and not yet let out.
const SLOTS: usize = if h264::dpb::MAX_FRAMES > hevc::dpb::MAX_PICTURES {
    h264::dpb::MAX_FRAMES
} else {
    hevc::dpb::MAX_PICTURES
};

/// Slices of one picture, as both readers hold them.
const MAX_SLICES: usize = if h264::MAX_SLICES > hevc::MAX_SLICES {
    h264::MAX_SLICES
} else {
    hevc::MAX_SLICES
};

/// Parameter sets kept at once: more than a stream uses, which is one of each.
const MAX_SETS: usize = 16;
/// The longest set kept; a longer one is refused, never cut.
const SET_BYTES: usize = 1024;

/// The kinds of set, in the order a description takes them.
const VPS: u8 = 0;
const SPS: u8 = 1;
const PPS: u8 = 2;

/// Parameter sets of 256x256 pictures, one stream of each combination a
/// declaration can name, for asking the system what it decodes. Above the
/// hardware's floor for ten bits, which refuses pictures under 128 rows.
const PROBE_H264: [&[u8]; 2] = [
    &[
        0x67, 0x4d, 0x40, 0x14, 0x95, 0x90, 0x10, 0x08, 0x6c, 0x04, 0x40, 0x00, 0x00, 0xfa, 0x00,
        0x00, 0x3a, 0x98, 0x38, 0x00, 0x00, 0x3d, 0x09, 0x00, 0x00, 0xf4, 0x24, 0x6e, 0xf2, 0xe0,
        0xa0,
    ],
    &[0x68, 0xeb, 0x8f, 0x20],
];
const PROBE_HEVC: [&[u8]; 3] = [
    &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x3f, 0x97, 0x02, 0x40,
    ],
    &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x3f, 0xa0, 0x08, 0x08, 0x04, 0x05, 0x96, 0x5d, 0x29, 0x08, 0x46, 0x45, 0xd5,
        0x0c, 0x04, 0x04, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x00, 0x79, 0x80, 0x17,
        0x79, 0x44, 0x00, 0x1e, 0x84, 0x80, 0x00, 0x7a, 0x12, 0x04,
    ],
    &[0x44, 0x01, 0xc1, 0xf7, 0xc0, 0xcc, 0x90],
];
const PROBE_HEVC_10: [&[u8]; 3] = [
    &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x03, 0x00, 0x5a, 0x97, 0x02, 0x40,
    ],
    &[
        0x42, 0x01, 0x01, 0x02, 0x20, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03, 0x00, 0x00,
        0x03, 0x00, 0x5a, 0xa0, 0x08, 0x08, 0x04, 0x04, 0xd9, 0x65, 0xd2, 0x90, 0x84, 0x64, 0x5d,
        0x50, 0xc0, 0x40, 0x40, 0x00, 0x00, 0x03, 0x00, 0x40, 0x00, 0x00, 0x07, 0x98, 0x01, 0x77,
        0x94, 0x40, 0x00, 0xf4, 0x24, 0x00, 0x00, 0xf4, 0x24, 0x04,
    ],
    &[0x44, 0x01, 0xc1, 0xf3, 0xc0, 0xcc, 0x90],
];
const PROBE_HEVC_444: [&[u8]; 3] = [
    &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x3c, 0x97, 0x02, 0x40,
    ],
    &[
        0x42, 0x01, 0x01, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9e, 0x08, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x3c, 0x90, 0x01, 0x01, 0x00, 0x80, 0xb2, 0xcb, 0xa5, 0x21, 0x08, 0xc8, 0xba, 0xa1,
        0x80, 0x80, 0x80, 0x00, 0x00, 0x03, 0x00, 0x80, 0x00, 0x00, 0x0f, 0x30, 0x02, 0xef, 0x28,
        0x80, 0x03, 0xd0, 0x90, 0x00, 0x0f, 0x42, 0x40, 0x80,
    ],
    &[0x44, 0x01, 0xc1, 0xf7, 0x18, 0x30, 0x0c, 0xc9],
];
const PROBE_HEVC_444_10: [&[u8]; 3] = [
    &[
        0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9c, 0x08, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x3c, 0x97, 0x02, 0x40,
    ],
    &[
        0x42, 0x01, 0x01, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x9c, 0x08, 0x00, 0x00, 0x03, 0x00,
        0x00, 0x3c, 0x90, 0x01, 0x01, 0x00, 0x80, 0x9b, 0x2c, 0xba, 0x52, 0x10, 0x8c, 0x8b, 0xaa,
        0x18, 0x08, 0x08, 0x00, 0x00, 0x03, 0x00, 0x08, 0x00, 0x00, 0x03, 0x00, 0xf3, 0x00, 0x2e,
        0xf2, 0x88, 0x00, 0x16, 0xe3, 0x40, 0x00, 0x16, 0xe3, 0x60, 0x80,
    ],
    &[0x44, 0x01, 0xc1, 0xf3, 0x18, 0x30, 0x0c, 0xc9],
];

/// Why a session could not be made or a unit decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A framework call failed while decoding, with what it answered.
    Framework(i32),
    /// The system makes no session for the stream's description.
    NoDecoder(i32),
    /// A stream this decoder must not be given, or a picture back in another
    /// layout, size or range than its stream's.
    NoProfile,
    /// A unit that could not be read.
    Stream,
    /// A unit, a set or a picture larger than this backend or the caller's
    /// planes hold.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Framework(status) => write!(f, "system decoder returned {status}"),
            Self::NoDecoder(status) => write!(f, "system decoder made no session, {status}"),
            Self::NoProfile => f.write_str("system decoder takes no stream of this shape"),
            Self::Stream => f.write_str("system decoder lost the stream"),
            Self::TooLarge => f.write_str("unit or picture larger than the decoder holds"),
        }
    }
}

impl std::error::Error for Error {}

type Result<T> = core::result::Result<T, Error>;

/// A non-negative duration in whole microseconds, saturated.
fn micros(ms: f64) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative duration in whole microseconds, saturated"
    )]
    let us = (ms * 1000.0).max(0.0).min(f64::from(u32::MAX)) as u32;
    us
}

/// What a stream is, as its sequence set says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    ten_bit: bool,
    full_chroma: bool,
}

impl Shape {
    /// The layout the decoder hands such a stream back in.
    const fn format(self) -> Format {
        match (self.ten_bit, self.full_chroma) {
            (false, false) => Format::Nv12,
            (true, false) => Format::P010,
            (false, true) => Format::Nv24,
            (true, true) => Format::P410,
        }
    }
}

/// The layout and range a buffer of the decoder's is in, as this crate names
/// them; none for a layout outside [`LAYOUTS`].
fn layout_of(code: OSType) -> Option<(Format, bool)> {
    LAYOUTS
        .iter()
        .find(|(layout, ..)| *layout == code)
        .map(|&(_, format, full_range)| (format, full_range))
}

/// One parameter set, as its bytes.
#[derive(Clone, Copy)]
struct Set {
    kind: u8,
    id: u8,
    len: usize,
    bytes: [u8; SET_BYTES],
}

/// The parameter sets a stream has carried, the last of each kind and
/// identifier replacing the one before, and whether any changed since a
/// description was made from them.
struct Sets {
    entries: [Option<Set>; MAX_SETS],
    changed: bool,
}

impl Sets {
    /// Keep `bytes` as the set of `kind` and `id`; a set already held with
    /// the same bytes changes nothing.
    fn store(&mut self, kind: u8, id: u8, bytes: &[u8]) -> Result<()> {
        if bytes.len() > SET_BYTES {
            return Err(Error::TooLarge);
        }
        let same = |set: &Set| set.kind == kind && set.id == id;
        let index = match self
            .entries
            .iter()
            .position(|e| e.as_ref().is_some_and(same))
        {
            Some(index) => index,
            None => self
                .entries
                .iter()
                .position(Option::is_none)
                .ok_or(Error::TooLarge)?,
        };
        let entry = self.entries.get_mut(index).ok_or(Error::TooLarge)?;
        if let Some(held) = entry.as_ref()
            && held.bytes.get(..held.len) == Some(bytes)
        {
            return Ok(());
        }
        let mut set = Set {
            kind,
            id,
            len: bytes.len(),
            bytes: [0; SET_BYTES],
        };
        set.bytes
            .get_mut(..bytes.len())
            .ok_or(Error::TooLarge)?
            .copy_from_slice(bytes);
        *entry = Some(set);
        self.changed = true;
        Ok(())
    }

    fn clear(&mut self) {
        self.entries = [None; MAX_SETS];
        self.changed = true;
    }

    /// Every set held, in the order a description takes them, as pointers
    /// and lengths; how many.
    fn collect(
        &self,
        pointers: &mut [*const u8; MAX_SETS],
        sizes: &mut [usize; MAX_SETS],
    ) -> usize {
        let mut count = 0;
        for kind in [VPS, SPS, PPS] {
            for set in self.entries.iter().flatten().filter(|s| s.kind == kind) {
                if let (Some(p), Some(s)) = (pointers.get_mut(count), sizes.get_mut(count)) {
                    *p = set.bytes.as_ptr();
                    *s = set.len;
                    count += 1;
                }
            }
        }
        count
    }
}

/// What the decoder's thread is given with each picture: where to put it,
/// and the table it takes its reference through.
struct Callback {
    vt: &'static Vt,
    picture: Handoff,
}

/// The decoder's answer for one picture, on its own thread: a reference of
/// this side's to the buffer, or the status that says why there is none.
extern "C" fn on_output(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: OSStatus,
    flags: VTDecodeInfoFlags,
    image: CVImageBufferRef,
    _presentation: CMTime,
    _duration: CMTime,
) {
    // SAFETY: the callback the session was made with, which the backend
    // keeps alive for longer than the session, and only reads.
    let callback = unsafe { &*refcon.cast_const().cast::<Callback>() };
    let bits = u32::from_ne_bytes(status.to_ne_bytes());
    if status != 0 || image.is_null() || flags & kVTDecodeInfo_FrameDropped != 0 {
        callback.picture.post(0, bits);
        return;
    }
    // SAFETY: a live buffer for the length of the callback; the reference
    // taken is the decoding thread's once posted.
    let held = unsafe { (callback.vt.buffer_retain)(image) };
    if !callback.picture.post(held.expose_provenance(), 0) {
        // A second picture for one call: nobody asked for it.
        // SAFETY: the reference taken above, given back.
        unsafe { (callback.vt.buffer_release)(held) };
    }
}

/// A session and the description it decodes samples under.
struct Session {
    vt: &'static Vt,
    session: NonNull<OpaqueVTDecompressionSession>,
    format: Object,
    /// Whether the hardware decodes for it.
    hardware: bool,
    /// Whether it has decoded a picture without an error.
    answered: bool,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a live session, invalidated once -- no callback runs after
        // the call returns -- then its reference given back.
        unsafe {
            (self.vt.session_invalidate)(self.session.as_ptr());
            (self.vt.release)(self.session.as_ptr().cast_const().cast());
        }
    }
}

/// An empty dictionary of framework objects.
fn dictionary(vt: &'static Vt) -> Option<Object> {
    // SAFETY: the framework's own callbacks for its objects; the reference is
    // this side's.
    unsafe {
        vt.own(
            (vt.dictionary_create_mutable)(
                core::ptr::null(),
                0,
                vt.dictionary_keys,
                vt.dictionary_values,
            )
            .cast_const()
            .cast(),
        )
    }
}

/// `value` under `key` in `dictionary`, which retains it.
fn set(vt: &'static Vt, dictionary: &Object, key: *const c_void, value: *const c_void) {
    // SAFETY: a mutable dictionary of this side's and two live objects.
    unsafe { (vt.dictionary_set_value)(dictionary.as_ptr().cast_mut().cast(), key, value) };
}

/// What a session hands out: the layouts named, every buffer backed by a
/// surface.
fn destination(vt: &'static Vt) -> Option<Object> {
    let attributes = dictionary(vt)?;
    // SAFETY: the framework's own callbacks; the reference is this side's.
    let layouts = unsafe {
        vt.own(
            (vt.array_create_mutable)(core::ptr::null(), 0, vt.array_values)
                .cast_const()
                .cast(),
        )
    }?;
    for (layout, ..) in LAYOUTS {
        let value = i32::from_ne_bytes(layout.to_ne_bytes());
        // SAFETY: a number made from a local; the reference is this side's.
        let number = unsafe {
            vt.own(
                (vt.number_create)(
                    core::ptr::null(),
                    CFNumberType::from(kCFNumberSInt32Type),
                    (&raw const value).cast(),
                )
                .cast(),
            )
        }?;
        // SAFETY: a mutable array of this side's and a live number it retains.
        unsafe { (vt.array_append_value)(layouts.as_ptr().cast_mut().cast(), number.as_ptr()) };
    }
    set(
        vt,
        &attributes,
        vt.pixel_format_key.cast(),
        layouts.as_ptr(),
    );
    let surface = dictionary(vt)?;
    set(
        vt,
        &attributes,
        vt.surface_properties_key.cast(),
        surface.as_ptr(),
    );
    Some(attributes)
}

/// A decoder choice that refuses the hardware: the system's software decoder
/// alone.
fn software_only(vt: &'static Vt) -> Option<Object> {
    let specification = dictionary(vt)?;
    set(
        vt,
        &specification,
        vt.enable_hardware_key.cast(),
        vt.boolean_false.cast(),
    );
    Some(specification)
}

/// A description of the stream the sets name.
fn describe(vt: &'static Vt, codec: Codec, sets: &[&[u8]]) -> Result<Object> {
    let mut pointers = [core::ptr::null::<u8>(); MAX_SETS];
    let mut sizes = [0usize; MAX_SETS];
    for ((p, s), set) in pointers.iter_mut().zip(sizes.iter_mut()).zip(sets) {
        *p = set.as_ptr();
        *s = set.len();
    }
    describe_raw(vt, codec, &pointers, &sizes, sets.len().min(MAX_SETS))
}

fn describe_raw(
    vt: &'static Vt,
    codec: Codec,
    pointers: &[*const u8; MAX_SETS],
    sizes: &[usize; MAX_SETS],
    count: usize,
) -> Result<Object> {
    let mut format: CMFormatDescriptionRef = core::ptr::null();
    // SAFETY: `count` sets, each `sizes[i]` bytes at `pointers[i]`, alive for
    // the call, which copies what it keeps; four-byte length prefixes.
    let status = unsafe {
        match codec {
            Codec::H264 => (vt.format_from_h264)(
                core::ptr::null(),
                count,
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                &raw mut format,
            ),
            Codec::H265 => (vt.format_from_hevc)(
                core::ptr::null(),
                count,
                pointers.as_ptr(),
                sizes.as_ptr(),
                4,
                core::ptr::null(),
                &raw mut format,
            ),
        }
    };
    if status != 0 {
        return Err(Error::Framework(status));
    }
    // SAFETY: the description the call made, its reference handed over.
    unsafe { vt.own(format.cast()) }.ok_or(Error::Framework(status))
}

/// Whether the hardware decodes for `session`.
fn using_hardware(vt: &'static Vt, session: VTDecompressionSessionRef) -> bool {
    let mut value: CFBooleanRef = core::ptr::null();
    // SAFETY: a live session; the value comes back with a reference.
    let status = unsafe {
        (vt.session_property)(
            session.cast_const().cast(),
            vt.using_hardware_key,
            core::ptr::null(),
            (&raw mut value).cast(),
        )
    };
    if status != 0 || value.is_null() {
        return false;
    }
    // SAFETY: a live boolean, read and then given back.
    unsafe {
        let answer = (vt.boolean_get_value)(value) != 0;
        (vt.release)(value.cast());
        answer
    }
}

/// A session for `format`, answering through `callback`, which must outlive
/// it.
fn make_session(
    vt: &'static Vt,
    format: Object,
    callback: &Callback,
    software: bool,
) -> Result<Session> {
    let destination = destination(vt).ok_or(Error::NoDecoder(-1))?;
    let specification = if software {
        Some(software_only(vt).ok_or(Error::NoDecoder(-1))?)
    } else {
        None
    };
    let record = VTDecompressionOutputCallbackRecord {
        decompressionOutputCallback: Some(on_output),
        decompressionOutputRefCon: core::ptr::from_ref(callback).cast_mut().cast(),
    };
    let mut session: VTDecompressionSessionRef = core::ptr::null_mut();
    // SAFETY: a live description and dictionaries; the record is copied by the
    // call; the session comes back with a reference.
    let status = unsafe {
        (vt.session_create)(
            core::ptr::null(),
            format.as_ptr().cast(),
            specification
                .as_ref()
                .map_or(core::ptr::null(), |s| s.as_ptr().cast()),
            destination.as_ptr().cast(),
            &raw const record,
            &raw mut session,
        )
    };
    if status != 0 {
        return Err(Error::NoDecoder(status));
    }
    let session = NonNull::new(session).ok_or(Error::NoDecoder(status))?;
    let hardware = using_hardware(vt, session.as_ptr());
    Ok(Session {
        vt,
        session,
        format,
        hardware,
        answered: false,
    })
}

/// What the system decodes, and of that what the hardware decodes: a session
/// made for each combination a declaration can name, nothing decoded. The
/// first session a process makes costs the framework's start, tens of
/// milliseconds, and each after it a few.
pub fn caps(vt: &'static Vt) -> (Caps, Caps) {
    let callback = Callback {
        vt,
        picture: Handoff::new(),
    };
    let ask = |codec: Codec, sets: &[&[u8]]| -> (bool, bool) {
        let Ok(format) = describe(vt, codec, sets) else {
            return (false, false);
        };
        match make_session(vt, format, &callback, false) {
            Ok(session) => (true, session.hardware),
            Err(_) => (false, false),
        }
    };
    let (h264, h264_hw) = ask(Codec::H264, &PROBE_H264);
    let (hevc, hevc_hw) = ask(Codec::H265, &PROBE_HEVC);
    let (hevc_10, hevc_10_hw) = ask(Codec::H265, &PROBE_HEVC_10);
    let (hevc_444, hevc_444_hw) = ask(Codec::H265, &PROBE_HEVC_444);
    let (hevc_444_10, hevc_444_10_hw) = ask(Codec::H265, &PROBE_HEVC_444_10);
    (
        Caps {
            h264,
            hevc,
            hevc_10,
            hevc_444,
            hevc_444_10,
        },
        Caps {
            h264: h264_hw,
            hevc: hevc_hw,
            hevc_10: hevc_10_hw,
            hevc_444: hevc_444_hw,
            hevc_444_10: hevc_444_10_hw,
        },
    )
}

/// What the reader staged: the slot the picture takes and its sequence's
/// shape, with its slices noted in `ranges`.
#[derive(Debug, Clone, Copy)]
struct Staged {
    slot: usize,
    shape: Shape,
    slices: usize,
}

/// The decoder.
pub struct Backend {
    vt: &'static Vt,
    codec: Codec,
    /// The system's software decoder alone: asked for, for a check of it,
    /// or taken when a session the system chose failed its first picture.
    software: bool,
    h264: Box<h264::Stream>,
    hevc: Box<hevc::Stream>,
    sets: Box<Sets>,
    /// Where each slice of the picture being decoded lies in its unit.
    ranges: [(usize, usize); MAX_SLICES],
    /// The sample: the picture's slices, each led by its length.
    sample: Box<[u8]>,
    /// The shape of the stream being decoded, once a picture has said.
    shape: Option<Shape>,
    /// Pictures decoded and not yet let out, by the reader's slot.
    held: [Option<PixelBuffer>; SLOTS],
    /// Before the callback the session answers through.
    session: Option<Session>,
    callback: Box<Callback>,
    /// Sessions made since this backend was.
    sessions: u32,
    /// The status a session the system chose failed its first picture with.
    refused: Option<i32>,
    /// The last decode's call and the last hand-over, in microseconds.
    pub decode_us: u32,
    pub readback_us: u32,
}

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("codec", &self.codec)
            .field("shape", &self.shape)
            .field("hardware", &self.hardware())
            .finish()
    }
}

impl Backend {
    /// A decoder for units of up to `unit_bytes`, through the hardware where
    /// the system offers it.
    pub fn new(vt: &'static Vt, unit_bytes: usize) -> Self {
        Self {
            vt,
            codec: Codec::H264,
            software: false,
            h264: Box::new(h264::Stream::new()),
            hevc: Box::new(hevc::Stream::new()),
            sets: Box::new(Sets {
                entries: [None; MAX_SETS],
                changed: true,
            }),
            ranges: [(0, 0); MAX_SLICES],
            // Each slice's start code, three bytes at the least, becomes a
            // four-byte length.
            sample: vec![0u8; unit_bytes + MAX_SLICES].into_boxed_slice(),
            shape: None,
            held: [const { None }; SLOTS],
            session: None,
            callback: Box::new(Callback {
                vt,
                picture: Handoff::new(),
            }),
            sessions: 0,
            refused: None,
            decode_us: 0,
            readback_us: 0,
        }
    }

    /// Sessions made since this backend was: one for a stream whose sets
    /// never change, however often it is rebuilt.
    pub fn sessions(&self) -> u32 {
        self.sessions
    }

    /// The status a session the system chose failed its first picture with,
    /// where one did and the software decoder took the stream; none
    /// otherwise.
    pub fn hardware_refused(&self) -> Option<i32> {
        self.refused
    }

    /// As [`new`](Self::new), refusing the hardware: the system's software
    /// decoder alone, for a check of it.
    pub fn software_only(vt: &'static Vt, unit_bytes: usize) -> Self {
        Self {
            software: true,
            ..Self::new(vt, unit_bytes)
        }
    }

    /// Whether the hardware decodes for the session in use; none before one
    /// is made.
    pub fn hardware(&self) -> Option<bool> {
        self.session.as_ref().map(|s| s.hardware)
    }

    /// Let every waiting picture out, as at the end of a stream; a test's
    /// need, since a live stream never ends this way.
    pub fn drain(&mut self) {
        self.h264.drain();
        self.hevc.drain();
    }

    /// The size and layout the pictures handed out have, once the stream has
    /// said: the active parameter set's visible size.
    pub fn output(&self) -> Option<(u32, u32, Format)> {
        let (width, height) = match self.codec {
            Codec::H264 => self.h264.active_sps()?.visible(),
            Codec::H265 => self.hevc.active_sps()?.visible(),
        };
        let format = self.shape?.format();
        (width > 0 && height > 0).then_some((width, height, format))
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

    fn release_held(&mut self) {
        self.held = [const { None }; SLOTS];
    }

    /// The session is not trusted past a fault: the next parameter sets make
    /// another.
    fn fault(&mut self, fault: Fault) -> Fault {
        self.session = None;
        self.sets.clear();
        self.release_held();
        fault
    }

    /// Keep the unit's parameter sets as their bytes.
    fn store_sets(&mut self, unit: &[u8]) -> Result<()> {
        for nal in Units::new(unit) {
            let Some(&head) = nal.bytes.first() else {
                continue;
            };
            let (kind, id) = match self.codec {
                Codec::H264 => {
                    let payload = nal.bytes.get(1..).unwrap_or(&[]);
                    match head & 0x1F {
                        7 => (
                            SPS,
                            h264::sps::parse(payload).map_err(|_| Error::Stream)?.id,
                        ),
                        8 => (PPS, first_ue(payload)?),
                        _ => continue,
                    }
                }
                Codec::H265 => {
                    let payload = nal.bytes.get(2..).unwrap_or(&[]);
                    match (head >> 1) & 0x3F {
                        32 => {
                            let id = BitReader::new(payload).u8(4).map_err(|_| Error::Stream)?;
                            (VPS, id)
                        }
                        33 => (
                            SPS,
                            hevc::sps::parse(payload).map_err(|_| Error::Stream)?.id,
                        ),
                        34 => (PPS, first_ue(payload)?),
                        _ => continue,
                    }
                }
            };
            self.sets.store(kind, id, nal.bytes)?;
        }
        Ok(())
    }

    /// Read the unit; the picture it stages, if any.
    fn read(&mut self, unit: &[u8]) -> Result<core::result::Result<Staged, Fed>> {
        match self.codec {
            Codec::H264 => {
                match self.h264.read(unit).map_err(|_| Error::Stream)? {
                    h264::Read::Nothing => return Ok(Err(self.pending())),
                    h264::Read::FormatChanged => return Ok(Err(Fed::FormatChanged)),
                    h264::Read::Picture => {}
                }
                let job = self.h264.job().ok_or(Error::Stream)?;
                let frame = job.current.structure == Structure::Frame;
                let slot = job.current.slot;
                let mut slices = 0;
                for (range, slice) in self
                    .ranges
                    .iter_mut()
                    .zip(job.slices.as_slice().iter().flatten())
                {
                    *range = (slice.offset, slice.len);
                    slices += 1;
                }
                // A field is half a picture this decoder hands out once a
                // pair; no host sends fields.
                if !frame {
                    self.h264.abandon();
                    return Err(Error::NoProfile);
                }
                // The reader takes eight-bit 4:2:0 alone.
                Ok(Ok(Staged {
                    slot,
                    shape: Shape {
                        ten_bit: false,
                        full_chroma: false,
                    },
                    slices,
                }))
            }
            Codec::H265 => {
                match self.hevc.read(unit).map_err(|_| Error::Stream)? {
                    hevc::Read::Nothing => return Ok(Err(self.pending())),
                    hevc::Read::FormatChanged => return Ok(Err(Fed::FormatChanged)),
                    hevc::Read::Picture => {}
                }
                let job = self.hevc.job().ok_or(Error::Stream)?;
                let shape = Shape {
                    ten_bit: job.sps.bit_depth_luma_minus8 > 0,
                    full_chroma: job.sps.chroma_format_idc == 3,
                };
                let extended = job.sps.is_range_extended();
                let slot = job.current.slot;
                let mut slices = 0;
                for (range, slice) in self
                    .ranges
                    .iter_mut()
                    .zip(job.slices.as_slice().iter().flatten())
                {
                    *range = (slice.offset, slice.len);
                    slices += 1;
                }
                // The range extensions' tools on a 4:2:0 stream are no
                // declaration's; refused, as on every decoder here.
                if !shape.full_chroma && extended {
                    self.hevc.abandon();
                    return Err(Error::NoProfile);
                }
                Ok(Ok(Staged {
                    slot,
                    shape,
                    slices,
                }))
            }
        }
    }

    fn abandon(&mut self) {
        match self.codec {
            Codec::H264 => self.h264.abandon(),
            Codec::H265 => self.hevc.abandon(),
        }
    }

    fn finish(&mut self) -> Result<()> {
        match self.codec {
            Codec::H264 => self.h264.finish(),
            Codec::H265 => self.hevc.finish(),
        }
        .map_err(|_| Error::Stream)
    }

    fn pending(&self) -> Fed {
        let waiting = match self.codec {
            Codec::H264 => self.h264.dpb.has_output(),
            Codec::H265 => self.hevc.dpb.has_output(),
        };
        if waiting {
            Fed::Picture
        } else {
            Fed::NeedMoreData
        }
    }

    /// A session for the sets held, the one in use where nothing changed or
    /// where it takes the new description.
    fn ensure_session(&mut self) -> Result<()> {
        if self.session.is_some() && !self.sets.changed {
            return Ok(());
        }
        let mut pointers = [core::ptr::null::<u8>(); MAX_SETS];
        let mut sizes = [0usize; MAX_SETS];
        let count = self.sets.collect(&mut pointers, &mut sizes);
        let format = describe_raw(self.vt, self.codec, &pointers, &sizes, count)?;
        if let Some(session) = self.session.as_mut() {
            // SAFETY: a live session and description.
            let takes = unsafe {
                (self.vt.session_accepts)(session.session.as_ptr(), format.as_ptr().cast())
            } != 0;
            if takes {
                session.format = format;
                self.sets.changed = false;
                return Ok(());
            }
        }
        // The session going is invalidated before another is made.
        self.session = None;
        self.session = Some(make_session(
            self.vt,
            format,
            &self.callback,
            self.software,
        )?);
        self.sessions += 1;
        self.sets.changed = false;
        Ok(())
    }

    /// The staged picture's slices into the sample, each led by its length;
    /// the sample's length.
    fn length_prefixed(&mut self, unit: &[u8], slices: usize) -> Result<usize> {
        let mut at = 0;
        for &(offset, len) in self.ranges.iter().take(slices) {
            let nal = unit.get(offset..offset + len).ok_or(Error::Stream)?;
            let prefix = u32::try_from(len)
                .map_err(|_| Error::TooLarge)?
                .to_be_bytes();
            let into = self
                .sample
                .get_mut(at..at + 4 + len)
                .ok_or(Error::TooLarge)?;
            let (head, body) = into.split_at_mut(4);
            head.copy_from_slice(&prefix);
            body.copy_from_slice(nal);
            at += 4 + len;
        }
        Ok(at)
    }

    /// Decode the sample, waiting for the picture; the picture, none where the
    /// decoder dropped it.
    fn decode_sample(&mut self, len: usize) -> Result<Option<PixelBuffer>> {
        let vt = self.vt;
        let session = self.session.as_ref().ok_or(Error::Stream)?;
        let mut block: CMBlockBufferRef = core::ptr::null_mut();
        // SAFETY: `len` bytes of the sample, which outlive the decode below; the
        // null allocator leaves them this side's. The block comes back with a
        // reference.
        let status = unsafe {
            (vt.block_buffer_with_memory)(
                core::ptr::null(),
                self.sample.as_mut_ptr().cast(),
                len,
                vt.allocator_null,
                core::ptr::null(),
                0,
                len,
                0,
                &raw mut block,
            )
        };
        if status != 0 {
            return Err(Error::Framework(status));
        }
        // SAFETY: the block the call made, its reference handed over.
        let block = unsafe { vt.own(block.cast_const().cast()) }.ok_or(Error::Framework(-1))?;
        let mut sample: CMSampleBufferRef = core::ptr::null_mut();
        // SAFETY: a live block and description; one sample of `len` bytes, no
        // timing; the sample comes back with a reference.
        let status = unsafe {
            (vt.sample_buffer_ready)(
                core::ptr::null(),
                block.as_ptr().cast_mut().cast(),
                session.format.as_ptr().cast(),
                1,
                0,
                core::ptr::null(),
                1,
                &raw const len,
                &raw mut sample,
            )
        };
        if status != 0 {
            return Err(Error::Framework(status));
        }
        // SAFETY: the sample the call made, its reference handed over.
        let sample = unsafe { vt.own(sample.cast_const().cast()) }.ok_or(Error::Framework(-1))?;
        // Anything a call before left is not this picture's.
        if let Some((stale, _)) = self.callback.picture.take() {
            // SAFETY: a reference the callback took for this side.
            drop(unsafe { PixelBuffer::adopt(vt, core::ptr::with_exposed_provenance_mut(stale)) });
        }
        let mut info: VTDecodeInfoFlags = 0;
        // SAFETY: a live session and sample; no flags, so the call returns once
        // the picture is out through the callback.
        let status = unsafe {
            (vt.session_decode)(
                session.session.as_ptr(),
                sample.as_ptr().cast_mut().cast(),
                0,
                core::ptr::null_mut(),
                &raw mut info,
            )
        };
        let answer = self.callback.picture.take();
        // SAFETY: a reference the callback took for this side, or null.
        let picture = answer.and_then(|(at, _)| unsafe {
            PixelBuffer::adopt(vt, core::ptr::with_exposed_provenance_mut(at))
        });
        if status != 0 {
            return Err(Error::Framework(status));
        }
        match answer {
            Some((_, bits)) if bits != 0 => {
                Err(Error::Framework(i32::from_ne_bytes(bits.to_ne_bytes())))
            }
            _ => Ok(picture),
        }
    }

    fn decode(&mut self, unit: &[u8]) -> Result<Fed> {
        self.store_sets(unit)?;
        let staged = match self.read(unit)? {
            Ok(staged) => staged,
            Err(fed) => return Ok(fed),
        };
        self.shape = Some(staged.shape);
        let picture = match self.decode_staged(unit, staged) {
            Ok(picture) => picture,
            Err(e) => {
                self.abandon();
                return Err(e);
            }
        };
        if let Some(buffer) = &picture
            && !self.matches(buffer, staged.shape)
        {
            self.abandon();
            return Err(Error::NoProfile);
        }
        // A picture the decoder dropped leaves nothing to hand out for its
        // slot; the reader's order is kept all the same.
        if let Some(held) = self.held.get_mut(staged.slot) {
            *held = picture;
        }
        self.finish()?;
        Ok(self.pending())
    }

    /// The staged picture through the session: its picture, none where the
    /// decoder dropped it.
    ///
    /// **A session the system chose that fails its first picture is the
    /// hardware refusing**, as one it cannot make is at the probe: the
    /// software decoder takes the picture again, and this backend's sessions
    /// from there. A stream the software decoder refuses too is refused as
    /// before.
    fn decode_staged(&mut self, unit: &[u8], staged: Staged) -> Result<Option<PixelBuffer>> {
        self.ensure_session()?;
        let len = self.length_prefixed(unit, staged.slices)?;
        let started = Time::now();
        let mut picture = self.decode_sample(len);
        if let Err(Error::Framework(status)) = picture
            && !self.software
            && self.session.as_ref().is_some_and(|s| !s.answered)
        {
            lowlat_common::log_warn!(
                "decode: the system's decoder refused its first picture, status={status}; the \
                 software decoder takes it"
            );
            self.software = true;
            self.refused = Some(status);
            self.session = None;
            self.ensure_session()?;
            picture = self.decode_sample(len);
        }
        if picture.is_ok()
            && let Some(session) = self.session.as_mut()
        {
            session.answered = true;
        }
        self.decode_us = micros(diff_ms(started, Time::now()));
        picture
    }

    /// Whether a buffer is the picture the reader says the stream is: its
    /// layout, its size and its range.
    fn matches(&self, buffer: &PixelBuffer, shape: Shape) -> bool {
        let ((width, height), full_range) = self.visible_and_range();
        let size = buffer.size();
        layout_of(buffer.format()) == Some((shape.format(), full_range))
            && usize::try_from(width).ok() == Some(size.0)
            && usize::try_from(height).ok() == Some(size.1)
    }

    /// The next picture the reader lets out that the decoder gave back.
    fn next_held(&mut self) -> Option<(PixelBuffer, i32)> {
        loop {
            let (slot, order) = match self.codec {
                Codec::H264 => {
                    let output = self.h264.next_output()?;
                    self.h264.dpb.taken(output.slot);
                    (output.slot, output.poc)
                }
                Codec::H265 => {
                    let output = self.hevc.next_output()?;
                    self.hevc.dpb.taken(output.slot);
                    (output.slot, output.poc)
                }
            };
            if let Some(buffer) = self.held.get_mut(slot).and_then(Option::take) {
                return Some((buffer, order));
            }
        }
    }

    /// What a buffer the reader let out is, as a picture.
    fn picture(&self, order: i32) -> Option<Picture> {
        let ((width, height), full_range) = self.visible_and_range();
        Some(Picture {
            format: self.shape?.format(),
            width,
            height,
            order,
            full_range,
        })
    }

    /// The next picture, lent whole: the decoder's own buffer, which goes
    /// back to its pool when the holder drops it.
    pub fn take_buffer(&mut self) -> core::result::Result<Option<(Picture, PixelBuffer)>, Fault> {
        let Some((buffer, order)) = self.next_held() else {
            return Ok(None);
        };
        let picture = self
            .picture(order)
            .ok_or_else(|| self.fault(Fault::Unrecoverable))?;
        self.readback_us = 0;
        Ok(Some((picture, buffer)))
    }
}

/// The first unsigned number of a set's payload: a picture set's own
/// identifier.
fn first_ue(payload: &[u8]) -> Result<u8> {
    let id = BitReader::new(payload).ue().map_err(|_| Error::Stream)?;
    u8::try_from(id).map_err(|_| Error::Stream)
}

/// A locked picture's planes into the caller's, row by row at each side's
/// pitch.
fn copy_planes(locked: &Locked, picture: Picture, out: &mut Planes<'_>) -> Result<()> {
    let format = picture.format;
    let width = usize::try_from(picture.width).map_err(|_| Error::TooLarge)?;
    let height = usize::try_from(picture.height).map_err(|_| Error::TooLarge)?;
    let planes = [
        (0, width * format.sample(), height),
        (
            1,
            format.chroma_row_bytes(width),
            format.chroma_rows(height),
        ),
    ];
    for (index, row_bytes, rows) in planes {
        let (base, pitch, held_rows) = locked.plane(index).ok_or(Error::Stream)?;
        if rows > held_rows || row_bytes > pitch {
            return Err(Error::Stream);
        }
        // SAFETY: the plane is locked for reading for as long as `locked`
        // lives and holds `held_rows` rows of `pitch` bytes.
        let source = unsafe { core::slice::from_raw_parts(base, pitch * held_rows) };
        let (to, to_pitch) = if index == 0 {
            (&mut *out.y, out.y_pitch)
        } else {
            (&mut *out.uv, out.uv_pitch)
        };
        for row in 0..rows {
            let from = source
                .get(row * pitch..row * pitch + row_bytes)
                .ok_or(Error::Stream)?;
            to.get_mut(row * to_pitch..row * to_pitch + row_bytes)
                .ok_or(Error::TooLarge)?
                .copy_from_slice(from);
        }
    }
    Ok(())
}

impl Decoder for Backend {
    fn build(&mut self, header: &VideoHeader) -> core::result::Result<(), Fault> {
        // Another codec's sets describe nothing of this stream's.
        if header.codec != self.codec {
            self.session = None;
            self.sets.clear();
        }
        self.codec = header.codec;
        self.shape = None;
        self.h264.reset();
        self.hevc.reset();
        self.release_held();
        Ok(())
    }

    fn feed(&mut self, unit: &[u8]) -> core::result::Result<Fed, Fault> {
        match self.decode(unit) {
            Ok(fed) => Ok(fed),
            // A stream the system will not decode would be refused again on
            // every keyframe asked for; nothing to ask.
            Err(Error::NoProfile | Error::NoDecoder(_) | Error::TooLarge) => {
                Err(self.fault(Fault::Fatal))
            }
            Err(Error::Framework(_) | Error::Stream) => Err(self.fault(Fault::Unrecoverable)),
        }
    }

    fn take(&mut self, out: &mut Planes<'_>) -> core::result::Result<Option<Picture>, Fault> {
        let Some((buffer, order)) = self.next_held() else {
            return Ok(None);
        };
        let started = Time::now();
        let picture = self
            .picture(order)
            .ok_or_else(|| self.fault(Fault::Unrecoverable))?;
        let Ok(locked) = buffer.lock() else {
            return Err(self.fault(Fault::Unrecoverable));
        };
        let copied = copy_planes(&locked, picture, out);
        drop(locked);
        self.readback_us = micros(diff_ms(started, Time::now()));
        match copied {
            Ok(()) => Ok(Some(picture)),
            Err(Error::TooLarge) => Err(self.fault(Fault::Fatal)),
            Err(_) => Err(self.fault(Fault::Unrecoverable)),
        }
    }

    fn destroy(&mut self) {
        // The pictures in flight go; the session and the sets are kept for a
        // rebuild that brings the same sets back.
        self.release_held();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every layout a session is asked for is one this crate names, in the
    /// range the code says, and nothing else is.
    #[test]
    fn every_layout_asked_for_is_named() {
        let named: Vec<_> = LAYOUTS.iter().filter_map(|l| layout_of(l.0)).collect();
        assert_eq!(named.len(), LAYOUTS.len());
        for format in [Format::Nv12, Format::P010, Format::Nv24, Format::P410] {
            assert!(named.contains(&(format, false)) && named.contains(&(format, true)));
        }
        assert_eq!(layout_of(0), None);
    }

    /// **A session the system chose that fails its first picture is the
    /// hardware refusing**: the software decoder takes the picture, and the
    /// backend's sessions from there. The failure is made by giving the
    /// first session a description it was not made for.
    #[test]
    fn a_first_picture_refused_goes_to_the_software_decoder() {
        let vt = lowlat_drivers::vt::load().unwrap();
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/fixtures/h264-ipp-cabac.bin"
        );
        let bytes = std::fs::read(path).unwrap();
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let unit = &bytes[4..4 + len];
        let mut backend = Backend::new(vt, 1 << 20);
        backend
            .build(&VideoHeader {
                frame_id: 1,
                width: 0,
                height: 0,
                codec: Codec::H264,
                rotation: lowlat_core::video::Rotation::None,
                ten_bit: false,
                locked: false,
                announced: false,
                metadata: false,
            })
            .unwrap();
        // As `decode` does, the session made before the picture goes to it.
        backend.store_sets(unit).unwrap();
        let Ok(Ok(staged)) = backend.read(unit) else {
            panic!("the first unit staged no picture");
        };
        backend.ensure_session().unwrap();
        backend.session.as_mut().unwrap().format = describe(vt, Codec::H265, &PROBE_HEVC).unwrap();
        let picture = backend
            .decode_staged(unit, staged)
            .expect("the software decoder took the picture");
        assert!(picture.is_some(), "no picture came back");
        assert_eq!(backend.hardware(), Some(false));
        assert!(backend.hardware_refused().is_some());
        assert_eq!(backend.sessions(), 2);
    }

    /// A set is kept by its kind and identifier: the same bytes again change
    /// nothing, other bytes replace it, and one past the slots or the length
    /// is refused rather than kept in part.
    #[test]
    fn a_set_is_kept_by_kind_and_identifier() {
        let mut sets = Sets {
            entries: [None; MAX_SETS],
            changed: false,
        };
        sets.store(SPS, 0, &[1, 2, 3]).unwrap();
        assert!(sets.changed);
        sets.changed = false;
        sets.store(SPS, 0, &[1, 2, 3]).unwrap();
        assert!(!sets.changed, "the same set again changed the sets");
        sets.store(SPS, 0, &[1, 2, 4]).unwrap();
        assert!(sets.changed, "other bytes did not replace the set");
        sets.store(PPS, 0, &[9]).unwrap();
        sets.store(VPS, 0, &[7]).unwrap();
        let mut pointers = [core::ptr::null::<u8>(); MAX_SETS];
        let mut sizes = [0usize; MAX_SETS];
        assert_eq!(sets.collect(&mut pointers, &mut sizes), 3);
        // The description's order: the video set, the sequence's, the
        // picture's.
        assert_eq!(&sizes[..3], &[1, 3, 1]);
        for id in 1..MAX_SETS {
            let id = u8::try_from(id).unwrap();
            let kept = sets.store(PPS, id, &[id]);
            assert_eq!(kept.is_ok(), usize::from(id) < MAX_SETS - 2, "id {id}");
        }
        assert_eq!(
            sets.store(SPS, 1, &[0; SET_BYTES + 1]),
            Err(Error::TooLarge)
        );
    }
}
