//! IO shell: sockets, threads, timers, wakeups. Drives the protocol core.
//!
//! A first-class, specified, tested component, not glue. See docs/02-io-shell.md.

//! This crate contains `unsafe`, and it is the first outside the concurrency
//! primitives to do so: batched receive and offload send are syscalls. Every
//! block is a thin wrapper with a local safety argument. `miri` cannot reach
//! any of them, because it cannot execute a syscall, so the sanitizer build
//! carries that weight instead (docs/08-testing.md 7).

#![cfg(target_os = "linux")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
// Tests may panic freely: a failing assertion is the point, and a fixture that
// cannot be built is a broken test rather than hostile input.
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

pub mod recv;
pub mod socket;

pub use recv::Batch;
pub use socket::{DEFAULT_TTL, RECV_BATCH, RECV_SLOT, Socket};
