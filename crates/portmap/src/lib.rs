//! Gateway port mapping: PCP, NAT-PMP and UPnP's Internet Gateway Device, for
//! the client and the host.
//!
//! A mapping keeps a stable port open on the gateway, so that the reflexive
//! candidate gathered through that port is reachable by anyone
//! (docs/03-connectivity.md 6). This crate is the one place that speaks HTTP,
//! and only to the gateway (docs/00-overview.md D3).
//!
//! The messages are pure, bytes in and values out, no socket and no clock;
//! [`Mapper`] is the one thread that sends them. Everything they read comes
//! from the local network, from any device that answers a search or holds the
//! gateway's address, so none of it is trusted: a length is checked before it
//! is read, a size is capped before anything is buffered past it, and nothing
//! that parses can panic.

// The system's own calls, in the per-platform module alone.
#![deny(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// Tests may panic freely: a failing assertion is the point, and a fixture that
// cannot be built is a broken test rather than hostile input. AGENTS.md 7.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        // Fixtures build small values from loop counters; a truncating cast
        // there is obviously fine and spelling out try_from obscures the test.
        clippy::cast_possible_truncation
    )
)]

pub mod desc;
pub mod http;
pub mod natpmp;
pub mod pcp;
pub mod route;
pub mod soap;
pub mod ssdp;
pub mod url;

mod error;
#[cfg(any(target_os = "linux", windows))]
mod mapper;

#[cfg(target_os = "linux")]
#[path = "sys/linux.rs"]
mod sys;
#[cfg(windows)]
#[path = "sys/windows.rs"]
mod sys;

#[cfg(all(test, any(target_os = "linux", windows)))]
mod fake;

pub use error::{Error, Result};
#[cfg(any(target_os = "linux", windows))]
pub use mapper::{Config, Mapper, Protocol, Status};

/// Replies captured from three gateways (`tests/data/`): an OpenWrt router, a
/// libupnp fibre gateway, and a Debian build of the same daemon as the first
/// in a test namespace.
#[cfg(test)]
mod captured {
    /// A captured HTTP reply, read as the mapper reads one.
    pub(crate) fn response(wire: &[u8]) -> crate::http::Response {
        let mut reader = crate::http::Reader::new(crate::http::DESCRIPTION_CAP);
        reader
            .push(wire)
            .unwrap()
            .expect("a captured reply is whole")
    }
}
