//! Input injection through the kernel input layer.
//!
//! Below the display server, so it works identically on every Linux display
//! stack and at the greeter. See docs/05-host.md section 7.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]
#![deny(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]

pub mod event;
pub mod usage;
