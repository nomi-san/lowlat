//! The host's own display driver: an indirect display adapter whose monitors
//! the system composes desktops for, plugged through a control interface
//! ([docs/11-drivers.md](../../../docs/11-drivers.md)).
//!
//! A user-mode driver. The system's driver host loads this library and calls
//! the user-mode framework's entry point, which the framework's stub library
//! carries and exports; that binds to the framework and calls [`DriverEntry`].
//! From there the framework and the indirect display class call back into
//! `driver`, `monitor` and `swapchain`. **A callback never unwinds into the
//! system's code**: each is a plain C function, so a panic ends the host
//! process, which the system starts again.

#![cfg(windows)]
#![deny(unsafe_op_in_unsafe_fn)]

mod driver;
mod ffi;
mod file;
mod frames;
mod log;
mod monitor;
mod swapchain;

use ffi::{NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG};

/// The oldest indirect display class this driver runs on: 1.4, the first that
/// lets a driver name the GPU to render on. The class's stub reads it.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
#[used]
pub static IddMinimumVersionRequired: ULONG = 4;

/// The driver's entry, called by the framework's own once it has bound.
///
/// # Safety
///
/// Called by the framework, with its own driver object and registry path.
#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn DriverEntry(
    driver: PDRIVER_OBJECT,
    registry: PCUNICODE_STRING,
) -> NTSTATUS {
    log::install();
    // SAFETY: the framework's own arguments, passed on.
    unsafe { driver::entry(driver, registry) }
}
