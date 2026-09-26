//! The host: orchestration of capture, encode, delivery and input.
//!
//! The public surface is the C ABI in `lowlat-sdk`, which is the only crate
//! that depends on this one for its boundary; the daemon uses it directly.

// Built where its platform's half is written, which is Linux so far
// (docs/impl-plan-windows.md). The session's negotiation and the packetiser
// touch no platform and build everywhere: the client's hermetic session runs
// against them.
#[cfg(target_os = "linux")]
pub mod admission;
#[cfg(target_os = "linux")]
pub(crate) mod audio;
#[cfg(target_os = "linux")]
pub mod cursor;
#[cfg(target_os = "linux")]
pub mod display;
#[cfg(target_os = "linux")]
pub mod events;
#[cfg(target_os = "linux")]
pub mod floor;
#[cfg(target_os = "linux")]
pub mod gate;
#[cfg(target_os = "linux")]
pub mod microphone;
#[cfg(target_os = "linux")]
pub mod padsink;
#[cfg(target_os = "linux")]
pub mod rate;
pub mod session;
#[cfg(target_os = "linux")]
pub mod stock;
#[cfg(target_os = "linux")]
pub mod stream;
#[cfg(target_os = "linux")]
pub mod timing;
pub mod video;

#[cfg(target_os = "linux")]
pub use admission::{Admission, Config, Event, HostCredentials, Outcome, Peer};

/// What a guest may drive, re-exported so an application setting it does not
/// have to name the injection crate.
/// The capture crate's own surface, for a caller that has to name a choice
/// this one does not settle.
#[cfg(target_os = "linux")]
pub mod capture {
    pub use lowlat_capture::Backend;
    /// The session's own account of where its displays are, re-exported so a
    /// program passing one along does not have to name the capture crate.
    pub use lowlat_capture::desktop::{Output, Placement, Watch, layout_of, place, tell};
    /// Asking the session to change a display, which only a program inside
    /// the session can do; the stream itself only ever follows the display.
    pub use lowlat_capture::mode;
}

#[cfg(target_os = "linux")]
pub mod inject {
    pub use lowlat_inject::event::Permissions;
    pub use lowlat_inject::uinput::console_takes_the_chord;
}
