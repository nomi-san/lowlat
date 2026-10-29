//! The host: orchestration of capture, encode, delivery and input.
//!
//! The public surface is the C ABI in `lowlat-sdk`, which is the only crate
//! that depends on this one for its boundary; the daemon uses it directly.

// It builds empty where no host runs (macOS, docs/impl-plan-macos.md).
#![cfg(any(target_os = "linux", windows))]

// What is the platform's -- the display, and what a stream is built on -- is a
// module per platform (docs/impl-plan-windows.md); the rest is written once.
pub mod admission;
pub(crate) mod audio;
pub mod cursor;
pub mod display;
pub mod events;
pub mod floor;
pub mod gate;
pub mod microphone;
pub mod padsink;
pub mod rate;
pub mod session;
pub mod stock;
pub mod stream;
pub mod timing;
pub mod video;

pub use admission::{Admission, Config, Event, HostCredentials, Outcome, Peer};

/// What a guest may drive, re-exported so an application setting it does not
/// have to name the injection crate.
/// The capture crate's own surface, for a caller that has to name a choice
/// this one does not settle.
pub mod capture {
    /// The session's own account of where its displays are, re-exported so a
    /// program passing one along does not have to name the capture crate.
    #[cfg(target_os = "linux")]
    pub use lowlat_capture::desktop::{Output, Watch, layout_of, place, tell};
    /// Asking the session to change a display, which only a program inside
    /// the session can do; the stream itself only ever follows the display.
    #[cfg(target_os = "linux")]
    pub use lowlat_capture::mode;
    pub use lowlat_capture::{Backend, Placement};
}

pub mod inject {
    pub use lowlat_inject::console_takes_the_chord;
    pub use lowlat_inject::event::Permissions;
}
