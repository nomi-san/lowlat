//! Orchestration and the public C ABI.
//!
//! The C ABI is the only public surface. Every extern "C" entry point catches
//! unwinding; see docs/06-api.md section 9.

// Phase 8 lands the ABI. Phases 4 through 7 land the orchestration.

pub mod abi;
pub mod admission;
pub(crate) mod audio;
pub mod cursor;
pub mod display;
pub mod events;
pub mod floor;
pub mod frames;
pub mod gate;
pub mod microphone;
pub mod rate;
pub mod session;
pub mod stock;
pub mod stream;
pub mod timing;
pub mod video;

pub use admission::{Admission, Config, Event, HostCredentials, Outcome, Peer};

/// What a guest may drive, re-exported so an application setting it does not
/// have to name the injection crate.
pub mod inject {
    pub use lowlat_inject::event::Permissions;
}
