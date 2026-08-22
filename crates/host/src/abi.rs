//! The public C ABI.
//!
//! The only public surface there is ([06-api.md](../../../docs/06-api.md)).
//! Naming follows the header rather than Rust convention, which is permitted
//! here and nowhere else.

#![allow(non_camel_case_types)]

use core::ffi::{CStr, c_char};

/// A status code.
///
/// **An integer with named constants rather than an enumeration**, and the
/// reason is soundness rather than taste: a status travels back into this
/// library -- ending a guest carries one as its reason -- and an application is
/// free to hand back a number we never defined. Reading an undefined
/// discriminant into a Rust enumeration is undefined behaviour, so the type
/// that crosses the boundary is one where every bit pattern is valid.
///
/// Zero succeeds, positive is a non-fatal condition, negative is an error, and
/// the error space is partitioned by subsystem so that a number says where it
/// came from without a lookup:
///
/// ```text
///   -1 to -99      the boundary itself: arguments, state, contained faults
///   -100 to -199   signaling and admission
///   -200 to -299   capture
///   -300 to -399   encode
///   -400 to -499   transport
/// ```
///
/// A value is assigned once and never reused, including for a condition that
/// is removed.
pub type lowlat_status = i32;

/// The call succeeded.
pub const LOWLAT_OK: lowlat_status = 0;
/// No event arrived within the timeout. Not an error.
pub const LOWLAT_TIMEOUT: lowlat_status = 1;

/// A fault was contained at the boundary. The handle no longer runs.
pub const LOWLAT_ERR_INTERNAL: lowlat_status = -1;
/// An argument was missing, out of range, or contradicted another.
pub const LOWLAT_ERR_INVALID_ARGUMENT: lowlat_status = -2;
/// The buffer was too small. What it would have taken has been written back,
/// and nothing has been consumed.
pub const LOWLAT_ERR_TOO_SMALL: lowlat_status = -3;
/// A previous call was contained at the boundary, so this handle is no longer
/// trusted to describe its own state. Only destroying it still works.
pub const LOWLAT_ERR_POISONED: lowlat_status = -4;

/// The major version, raised only when something already published changes.
pub const LOWLAT_ABI_MAJOR: u32 = 0;
/// The minor version, raised when surface is appended.
pub const LOWLAT_ABI_MINOR: u32 = 1;

/// Major and minor, packed.
///
/// **The one function whose signature can never change**, because it is what a
/// loader calls to decide whether it may call anything else.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_abi_version() -> u32 {
    (LOWLAT_ABI_MAJOR << 16) | LOWLAT_ABI_MINOR
}

/// Describe a status.
///
/// The pointer is to storage that outlives the library, so it is never freed
/// and never copied out of. An unrecognised value is described as one rather
/// than refused: this is what a caller reaches for while diagnosing, and
/// returning nothing there is the least useful thing it could do.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_status_string(status: lowlat_status) -> *const c_char {
    let text: &CStr = match status {
        LOWLAT_OK => c"ok",
        LOWLAT_TIMEOUT => c"no event within the timeout",
        LOWLAT_ERR_INTERNAL => c"a fault was contained at the boundary",
        LOWLAT_ERR_INVALID_ARGUMENT => c"an argument was not usable",
        LOWLAT_ERR_TOO_SMALL => c"the buffer was too small",
        LOWLAT_ERR_POISONED => c"the handle is poisoned",
        _ => c"unknown status",
    };
    text.as_ptr()
}

/// Panic on purpose, and prove the boundary contains it.
///
/// **Exported by the shipped library rather than hidden behind a build
/// option**, because what has to be tested is that *this* object still
/// unwinds. Building it to abort on panic silently disables containment
/// everywhere, and the same code linked into a test binary answers for the
/// test's build rather than for this one.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_debug_panic() -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        panic!("deliberate panic, to prove the boundary contains one")
    })
}

/// Run one entry point's body with unwinding contained.
///
/// A panic crossing an `extern "C"` boundary is undefined behaviour and this
/// library loads into processes we do not control, so every entry point that
/// runs any of our code goes through here.
///
/// **Unwind safety is asserted rather than proven**, and what makes that sound
/// is the poisoning that arrives with the handle: state a panic may have left
/// half-written is never read again, because every later call on that handle
/// is refused before it reaches this point.
fn guard<T>(contained: T, call: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(value) => value,
        Err(_) => {
            lowlat_common::log_error!("abi: a call panicked, contained at the boundary");
            contained
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The version is packed, not added.** A minor of 1 and a major of 1 are
    /// different versions, and a loader that compares a sum accepts one for
    /// the other.
    #[test]
    fn the_version_packs_major_above_minor() {
        let packed = lowlat_abi_version();
        assert_eq!(packed >> 16, LOWLAT_ABI_MAJOR);
        assert_eq!(packed & 0xffff, LOWLAT_ABI_MINOR);
    }

    /// **Every status describes itself, and an undefined one still answers.**
    /// A caller reaches for this while something is already wrong, so a null
    /// pointer here costs the diagnosis it was called for.
    #[test]
    fn every_status_describes_itself_and_so_does_one_we_never_defined() {
        for status in [
            LOWLAT_OK,
            LOWLAT_TIMEOUT,
            LOWLAT_ERR_INTERNAL,
            LOWLAT_ERR_INVALID_ARGUMENT,
            LOWLAT_ERR_TOO_SMALL,
            LOWLAT_ERR_POISONED,
        ] {
            let text = lowlat_status_string(status);
            assert!(!text.is_null());
            // Safe: the pointer is to a literal with static storage.
            let text = unsafe { CStr::from_ptr(text) };
            assert!(
                !text.to_bytes().is_empty(),
                "status {status} describes itself as nothing"
            );
        }
        let unknown = unsafe { CStr::from_ptr(lowlat_status_string(-31337)) };
        assert_eq!(unknown.to_bytes(), b"unknown status");
    }

    /// **A panic is contained and reported, not propagated.** The test that
    /// matters runs against the built shared object, in `tests/abi.rs`; this
    /// one only says the contained value is the documented one.
    #[test]
    fn a_panic_becomes_a_status() {
        assert_eq!(lowlat_debug_panic(), LOWLAT_ERR_INTERNAL);
    }
}
