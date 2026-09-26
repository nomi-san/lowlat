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

// The devices are built where their platform's half is written, which is Linux
// so far (docs/impl-plan-windows.md); what a guest's input is before it lands
// on one -- the events, the pads, the usage table -- builds everywhere.
pub mod event;
pub mod gamepad;
#[cfg(target_os = "linux")]
pub mod uhid;
#[cfg(target_os = "linux")]
pub mod uinput;
pub mod usage;

// The devices a guest's input lands on and what they hand back, by the names
// the guest loop uses whatever the platform; the kernel's input layer
// provides them here.
#[cfg(target_os = "linux")]
pub use uhid::WRITTEN_MAX;
#[cfg(target_os = "linux")]
pub use uinput::{Devices, Forward, Forwarded, PadWritten};
