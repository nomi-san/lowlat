//! IO shell: sockets, threads, timers, wakeups. Drives the protocol core.
//!
//! A first-class, specified, tested component, not glue. See docs/02-io-shell.md.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

// Phase 3 lands the contents.
