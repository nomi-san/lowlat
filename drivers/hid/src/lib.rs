//! The host's own input driver: a filter above the system's virtual HID
//! framework, which makes the HID devices the host asks for -- a keyboard, a
//! mouse, a DualShock 4 and a DualSense -- each bound to the handle that made
//! it ([docs/11-drivers.md](../../../docs/11-drivers.md) section 2).
//!
//! A user-mode driver. The system's driver host loads this library and calls
//! the user-mode framework's entry point, which the framework's stub library
//! carries and exports; that binds to the framework and calls [`DriverEntry`].
//! From there the framework calls back into `driver`, and the virtual HID
//! framework into `device`. **A callback never unwinds into the system's
//! code**: each is a plain C function, so a panic ends the host process, which
//! the system starts again.

#![cfg(windows)]
#![deny(unsafe_op_in_unsafe_fn)]

mod device;
mod driver;
mod ffi;
mod log;

use ffi::{NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG};

/// The oldest user-mode framework this driver binds to: 2.25, the version its
/// declarations are generated from. The framework's stub reads it.
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
#[used]
pub static WdfMinimumVersionRequired: ULONG = 25;

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
