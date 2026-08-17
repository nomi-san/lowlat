//! Shared primitives for every lowlat crate.
//!
//! Nothing here is protocol-aware. The rules these types exist to enforce are
//! in docs/02-io-shell.md; each one is a production failure from an earlier
//! implementation, and the comments say which.

pub mod bytes;
pub mod clock;
pub mod dynlib;
pub mod log;
// RFC 1982 sequence arithmetic lives in `lowlat-core`: it is protocol
// semantics, and the core cannot reach it here because this crate is std.
pub mod spsc;
pub mod wait;

#[cfg(feature = "alloc-counter")]
pub mod alloc_counter;

pub mod sync;
