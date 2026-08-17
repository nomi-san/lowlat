//! Orchestration and the public C ABI.
//!
//! The C ABI is the only public surface. Every extern "C" entry point catches
//! unwinding; see docs/06-api.md section 9.

// Phase 8 lands the ABI. Phases 4 through 7 land the orchestration.

pub mod admission;
pub mod frames;
pub mod gate;
pub mod video;

pub use admission::{Admission, Config, Event, HostCredentials, Outcome, Peer};
