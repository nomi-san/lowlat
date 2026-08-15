//! Shared primitives for every lowlat crate.
//!
//! Nothing here is protocol-aware. The rules these types exist to enforce are
//! in docs/02-io-shell.md; each one is a production failure from an earlier
//! implementation, and the comments say which.

pub mod bytes;
pub mod clock;
pub mod log;
pub mod seq;
pub mod spsc;
pub mod wait;

#[cfg(feature = "alloc-counter")]
pub mod alloc_counter;

mod sync;
