//! Sans-IO protocol core: wire, channels, rings, crypto, recovery, connectivity.
//!
//! This crate is `no_std` by design, not by portability ambition. It removes
//! `std::time`, `std::net`, `std::thread`, and the allocator by construction,
//! so a sans-IO violation is a compile error rather than a review finding.
//! See docs/00-overview.md D4.
//!
//! Nothing here reads a clock, owns a socket, spawns a thread, or generates a
//! random number. Time arrives as a parameter and nonces are derived, which is
//! what makes replay and simulation reproducible.

#![no_std]
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
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )
)]

// The crate never uses the standard library. Tests may: fixtures are
// allowed to allocate, and a test that cannot build a fixture is a broken
// test rather than hostile input.
#[cfg(test)]
extern crate std;

pub mod channel;
pub mod congestion;
pub mod control;
pub mod envelope;
pub mod error;
pub mod message;
pub mod packet;
pub mod pmtu;
pub mod send;
pub mod seq;
pub mod session;
pub mod video;

pub use error::{Error, Result};

/// Absolute ceiling on an emitted datagram, in bytes.
///
/// A peer's reassembly copy is bounded only by its receive buffer, so this is a
/// safety property and not merely a functional one. See docs/01-protocol.md 8.
pub const MAX_DATAGRAM: usize = 2000;

/// Default and floor datagram size: 1200 of cleartext plus the 29-byte envelope.
pub const DEFAULT_DATAGRAM: usize = 1229;

/// Largest cleartext a datagram at the ceiling can carry.
pub const MAX_CLEARTEXT: usize = MAX_DATAGRAM - envelope::ENVELOPE_LEN;

// The ceiling is a peer-safety bound, so make it impossible to raise the
// default past it by editing one constant.
const _: () = assert!(DEFAULT_DATAGRAM <= MAX_DATAGRAM);
const _: () = assert!(MAX_CLEARTEXT + envelope::ENVELOPE_LEN == MAX_DATAGRAM);
