//! Video decoders: the bitstream read by us, the pictures decoded by the
//! device.
//!
//! The device interfaces on this platform decode a picture from its
//! parameters and its slices, and reading those out of the bitstream is the
//! decoder's own job: parameter sets, slice headers, picture order, the
//! reference pictures a slice names and the buffer that holds them. That
//! reading is what this crate mostly is (docs/10-client.md section 5.1); the
//! backends beneath it hand the device what it asks for and read the picture
//! back.
//!
//! **Nothing here allocates per unit.** The parser's state, the picture
//! buffer and the parameter staging are fixed arrays sized by the coding
//! standards; a unit that needs more than they hold is refused, never
//! truncated.

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )
)]

pub mod bits;
pub mod h264;
pub mod hevc;
pub mod nal;
pub mod nvdec;
pub mod vaapi;

use lowlat_core::video::VideoHeader;

/// What a device decodes, asked once at creation. Shared by every backend,
/// and what the application's preferences are masked with before anything
/// is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    pub h264: bool,
    pub hevc: bool,
    pub hevc_10: bool,
    /// Full chroma at eight and ten bits.
    pub hevc_444: bool,
    pub hevc_444_10: bool,
}

impl Caps {
    /// Whether anything at all can be decoded.
    pub fn any(&self) -> bool {
        self.h264 || self.hevc || self.hevc_10 || self.hevc_444 || self.hevc_444_10
    }
}

/// What a backend reports for one unit it was fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fed {
    /// At least one picture is ready to be taken with [`Decoder::take`].
    Picture,
    /// Nothing came out this time, and nothing is wrong: the unit was
    /// consumed and a later one completes it.
    NeedMoreData,
    /// The stream's format changed under a decoder built for another. The
    /// unit was not decoded; a fresh decoder takes it.
    FormatChanged,
}

/// A backend failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// This decoder cannot continue, and a fresh one built from the next
    /// keyframe can. The one case a client asks the host for that keyframe.
    Unrecoverable,
    /// No decoder can continue: the device is gone or was never usable. The
    /// stream ends, with this named.
    Fatal,
}

/// The layout a picture is read back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Eight bits: a luma plane and an interleaved chroma plane at half the
    /// rows.
    Nv12,
    /// Ten bits in sixteen-bit samples, the value in the high bits; the same
    /// two planes.
    P010,
    /// Eight bits, full chroma: three planes of the picture's size.
    Yuv444,
    /// Ten bits in sixteen-bit samples, the value in the high bits; the same
    /// three planes.
    Yuv444_16,
}

impl Format {
    /// Bytes per sample.
    pub const fn sample(self) -> usize {
        match self {
            Self::Nv12 | Self::Yuv444 => 1,
            Self::P010 | Self::Yuv444_16 => 2,
        }
    }

    /// Whether chroma is at the picture's own resolution, in two planes.
    pub const fn full_chroma(self) -> bool {
        matches!(self, Self::Yuv444 | Self::Yuv444_16)
    }

    /// Rows in each chroma plane for a picture `height` rows tall.
    pub const fn chroma_rows(self, height: usize) -> usize {
        if self.full_chroma() {
            height
        } else {
            height.div_ceil(2)
        }
    }

    /// Bytes a chroma row carries for a picture `width` samples wide: two
    /// interleaved samples per pair of pixels, or one per pixel per plane,
    /// which come to the same count.
    pub const fn chroma_row_bytes(self, width: usize) -> usize {
        width * self.sample()
    }

    /// The layout for a stream's depth and chroma.
    pub const fn of(ten_bit: bool, full_chroma: bool) -> Self {
        match (ten_bit, full_chroma) {
            (false, false) => Self::Nv12,
            (true, false) => Self::P010,
            (false, true) => Self::Yuv444,
            (true, true) => Self::Yuv444_16,
        }
    }
}

/// Where a picture is read back to: the planes the caller owns.
///
/// The pitches are the caller's; a backend writes `width` samples of each
/// of `height` luma rows and the format's chroma rows and touches nothing
/// past them. `uv` is the interleaved chroma plane, or the first of the
/// two chroma planes at full chroma, where `v` is the second; a two-plane
/// layout leaves `v` empty.
#[derive(Debug)]
pub struct Planes<'a> {
    pub y: &'a mut [u8],
    pub y_pitch: usize,
    pub uv: &'a mut [u8],
    pub uv_pitch: usize,
    pub v: &'a mut [u8],
    pub v_pitch: usize,
}

/// What a decoded picture is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// The order the picture holds in its stream's output, from the
    /// bitstream's own count; it says which picture this is, not when to
    /// show it.
    pub order: i32,
}

/// A video decoder, as the client's feed drives one.
///
/// **Built and destroyed by the feed, never by itself.** The feed owns the
/// decision of when a decoder exists, because the one request a client may
/// make of a host is paired with that decision and must be made exactly once.
pub trait Decoder {
    /// Create the backend for what the header names. Called only while none
    /// exists.
    fn build(&mut self, header: &VideoHeader) -> Result<(), Fault>;
    /// Decode one access unit: the bitstream after the video header.
    fn feed(&mut self, unit: &[u8]) -> Result<Fed, Fault>;
    /// Read the next ready picture into the caller's planes, if one is
    /// ready. A unit that reported [`Fed::Picture`] has at least one; a
    /// flush at a refresh can leave more, taken in order.
    fn take(&mut self, out: &mut Planes<'_>) -> Result<Option<Picture>, Fault>;
    /// Tear the backend down. Called only while one exists.
    fn destroy(&mut self);
}

/// Why a unit could not be read. Diagnostic: the feed sees a [`Fault`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ParseError {
    /// The bitstream ended inside a syntax element.
    Truncated,
    /// A value outside what the standard allows.
    OutOfRange,
    /// A slice named a parameter set the stream never carried.
    NoParameterSet,
    /// More slices, or more of something else, than this decoder holds.
    TooMany,
    /// Syntax the standard defines and no device here decodes.
    Unsupported,
}
