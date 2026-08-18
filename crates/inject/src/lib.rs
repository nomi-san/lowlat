//! Input injection through the kernel input layer.
//!
//! Below the display server, so it works identically on every Linux display
//! stack and at the greeter. See docs/05-host.md section 7.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_debug_implementations)]
#![deny(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
// Tests may panic freely: a failing assertion is the point, and a fixture that
// cannot be built is a broken test rather than hostile input. AGENTS.md 7.
#![cfg_attr(
    test,
    allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )
)]

pub mod event;
pub mod gamepad;
pub mod uinput;
pub mod usage;
