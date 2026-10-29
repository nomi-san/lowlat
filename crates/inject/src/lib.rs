//! Input injection through the kernel input layer, and on Windows through the
//! system's batch input call and a virtual pad bus.
//!
//! Below the display server, so it works identically on every Linux display
//! stack and at the greeter. See docs/05-host.md section 7.

// The host's alone, so it builds empty where no host runs (macOS,
// docs/impl-plan-macos.md).
#![cfg(any(target_os = "linux", windows))]
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

// The devices are each platform's; what a guest's input is before it lands on
// one -- the events and the pads -- builds everywhere. The usage table is the
// platform's own translation of a keyboard's keys, under one name.
pub mod answers;
pub mod event;
pub mod gamepad;
pub mod handback;
#[cfg(windows)]
pub mod hid;
#[cfg(windows)]
mod scancode;
#[cfg(target_os = "linux")]
pub mod uhid;
#[cfg(target_os = "linux")]
pub mod uinput;
#[cfg(not(windows))]
pub mod usage;
#[cfg(windows)]
#[path = "usage_windows.rs"]
pub mod usage;
// Generated from the pad bus's header, which follows C naming and declares
// more than is called; scoped to the generated module alone. Its layout
// assertions index at compile time, which the crate's own deny names, and a
// group allow does not outrank a lint named explicitly.
#[cfg(windows)]
#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    missing_debug_implementations,
    unreachable_pub,
    clippy::all,
    clippy::indexing_slicing
)]
mod vigem;
#[cfg(windows)]
mod windows;

// The devices a guest's input lands on and what they hand back, by the names
// the guest loop uses whatever the platform; the kernel's input layer
// provides them on Linux.
pub use handback::{Forward, Forwarded, PadWritten, WRITTEN_MAX};
#[cfg(target_os = "linux")]
pub use uinput::{Devices, console_takes_the_chord};
#[cfg(windows)]
pub use windows::{Devices, console_takes_the_chord};
