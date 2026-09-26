//! The real desktop as a frame source for the encode loop.
//!
//! What the loop and the boundary see of a display on any platform: the
//! outputs it lists, what a pre-flight found, and what a capture produced.
//! The display itself -- capture, conversion and the encoder's registration
//! on one device -- is the platform's, under the same names on each.

use lowlat_capture::Placement;
use lowlat_common::clock::Time;

#[cfg(target_os = "linux")]
#[path = "display/linux.rs"]
mod sys;

pub(crate) use sys::driver_of;
pub use sys::{Display, Error, Register, Registration};

/// One output a host can be asked to capture.
#[derive(Debug, Clone)]
pub struct Selectable {
    /// What to ask for, such as `card0:DP-2`.
    pub id: String,
    /// The connector's own name, which is what the session knows it by.
    pub connector: String,
    pub width: u32,
    pub height: u32,
    /// How many times a second it presents, or zero when the device will not
    /// say.
    pub refresh_hz: u32,
    /// Where it sits in the desktop, when a session describes it.
    pub place: Option<Placement>,
}

/// What a pre-flight found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capturable {
    /// A display is lit and its framebuffer can be reached.
    Yes,
    /// Nothing is lit: no display, or no session driving one.
    NothingLit,
    /// Something is lit and its framebuffer cannot be reached, which is the
    /// privilege rather than the hardware.
    NotReachable,
}

/// Which of these outputs a published capture checksum names.
///
/// **The checksum is how the loop says what it is capturing without a lock**
/// ([`crate::stream`]), and this is the other half: a caller that can
/// enumerate the outputs gets the one being captured without ever handling the
/// encoding. Nothing when the loop has not opened a display, or when what it
/// opened is no longer in the list.
#[must_use]
pub fn captured(listed: &[Selectable], checksum: u32) -> Option<&Selectable> {
    if checksum == 0 {
        return None;
    }
    listed
        .iter()
        .find(|output| lowlat_core::crc32::of(output.id.as_bytes()) == checksum)
}

/// What one [`Display::acquire`] and [`Display::converted`] pair produced.
#[derive(Debug, Clone, Copy)]
pub struct Acquired {
    /// When the picture was taken, which is what every latency figure is
    /// measured from.
    pub at: Time,
    /// **False means this picture is the previous one, byte for byte.** A
    /// caller may skip everything downstream of it; nothing is skipped here,
    /// because the conversion is what produced the answer.
    pub changed: bool,
}
