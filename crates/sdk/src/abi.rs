//! The shared part of the public C ABI: what every build exports.
//!
//! The only public surface there is ([06-api.md](../docs/06-api.md)).
//! Naming follows the header rather than Rust convention, which is permitted
//! here and nowhere else. The host half lives in [`host`] behind its feature;
//! nothing here takes a handle, so nothing here depends on either half.

#![allow(non_camel_case_types)]

use core::ffi::{CStr, c_char, c_void};

use lowlat_status::*;

#[cfg(any(feature = "host", feature = "client"))]
pub mod shared;
#[cfg(any(feature = "host", feature = "client"))]
pub use shared::*;

#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "host")]
pub use host::*;

#[cfg(feature = "client")]
pub mod client;
#[cfg(feature = "client")]
pub use client::*;

/// A status code.
///
/// **An enumeration for the names and a plain integer wherever one is
/// accepted.** Grouping the codes under a type is what tells a reader that
/// `LOWLAT_TIMEOUT` is a status and `LOWLAT_ATTEMPT_MAX` is a size; taking one
/// back by value as this type would be something else entirely, because
/// reading a discriminant nothing defined is undefined behaviour and an
/// application is free to hand back any integer it has.
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
///   -500 to -599   decode
/// ```
///
/// A value is assigned once and never reused, including for a condition that
/// is removed.
// **`repr(C)` rather than `repr(i32)`, and every enumeration here follows
// it.** Naming the width makes cbindgen state it in C, which only C23 and C++
// have syntax for, so the header grows a `__STDC_VERSION__` fork and the same
// name means an enumeration under one standard and an integer under another.
// `repr(C)` is whatever the platform's C compiler picks, which is what the
// application is compiling with anyway; `alone.c` asserts it is four bytes.
//
// Not a doc comment, because it describes this side of the boundary and the
// header is written for the other one (AGENTS.md 1a).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_status {
    /// The call succeeded.
    LOWLAT_OK = 0,
    /// No event arrived within the timeout. Not an error.
    LOWLAT_TIMEOUT = 1,
    /// A fault was contained at the boundary. The handle no longer runs.
    LOWLAT_ERR_INTERNAL = -1,
    /// An argument was missing, out of range, or contradicted another.
    LOWLAT_ERR_INVALID_ARGUMENT = -2,
    /// The buffer was too small. What it would have taken has been written
    /// back, and nothing has been consumed.
    LOWLAT_ERR_TOO_SMALL = -3,
    /// A previous call was contained at the boundary, so this handle is no
    /// longer trusted to describe its own state. Only destroying it still
    /// works.
    LOWLAT_ERR_POISONED = -4,
    /// This handle is already hosting. Stopping first is the way to start
    /// again with a different configuration.
    LOWLAT_ERR_ALREADY_STARTED = -5,
    /// This handle is not hosting, so there is nothing for the call to act on.
    LOWLAT_ERR_NOT_STARTED = -6,
    /// The application already holds as many pictures as it may; one has to
    /// be released before another is acquired.
    LOWLAT_ERR_TOO_MANY_HELD = -7,

    /// Every seat is taken. **The offer should be declined**, not left
    /// unanswered: silence reads to a peer as a host still thinking about it.
    LOWLAT_ERR_AT_CAPACITY = -100,
    /// No attempt with that identifier.
    LOWLAT_ERR_UNKNOWN_ATTEMPT = -101,
    /// The attempt has already been approved.
    LOWLAT_ERR_ALREADY_BEGUN = -102,
    /// Withdrawn before it was registered, so it was over before it began. A
    /// withdrawal can overtake the offer it withdraws.
    LOWLAT_ERR_WITHDRAWN = -103,
    /// A socket could not be opened, or a thread could not be started.
    LOWLAT_ERR_IO = -104,
    /// Credentials could not be produced.
    LOWLAT_ERR_CRYPTO = -105,
    /// No guest with that number is connected.
    LOWLAT_ERR_UNKNOWN_GUEST = -106,
    /// A browser's offer carried no certificate digest, or one that is not
    /// a SHA-256 digest: nothing its handshake could be checked against.
    LOWLAT_ERR_FINGERPRINT = -107,

    /// Nothing is lit. There is no display to capture: a headless machine, or
    /// one whose session has not started.
    LOWLAT_ERR_NO_DISPLAY = -200,
    /// A display is lit and its framebuffer cannot be reached, which is what
    /// this process is allowed to do rather than what the machine has.
    LOWLAT_ERR_DISPLAY_UNREACHABLE = -201,

    /// The decoder's runtime library is not on the machine.
    LOWLAT_ERR_NO_DECODER_RUNTIME = -500,
    /// No render node opened for the decoder: none named opens, or none at
    /// all does.
    LOWLAT_ERR_NO_DECODER_DEVICE = -501,
    /// The device opened and decodes none of the profiles a stream could use.
    LOWLAT_ERR_NO_DECODER_PROFILE = -502,
    /// The decoder or the frame kind asked for is not in this build.
    LOWLAT_ERR_DECODER_UNSUPPORTED = -503,
    /// A codec library was found and is not one this build may load: it
    /// answered a licence other than the LGPL -- or than the GPL as well, in
    /// a build reporting `LOWLAT_FEATURE_GPL_LIBAVCODEC` -- and was closed
    /// unused.
    LOWLAT_ERR_NO_DECODER_LICENCE = -504,
}

/// The major version, raised only when something already published changes.
pub const LOWLAT_ABI_MAJOR: u32 = 0;
/// The minor version, raised when surface is appended.
pub const LOWLAT_ABI_MINOR: u32 = 16;

/// Major and minor, packed.
///
/// **The one function whose signature can never change**, because it is what a
/// loader calls to decide whether it may call anything else.
///
/// @returns The major version in the high sixteen bits, the minor in the low.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_abi_version() -> u32 {
    (LOWLAT_ABI_MAJOR << 16) | LOWLAT_ABI_MINOR
}

/// The host half is in this build: every `lowlat_host_*` entry point exists.
pub const LOWLAT_FEATURE_HOST: u32 = 1;
/// The client half is in this build: every `lowlat_client_*` entry point exists.
pub const LOWLAT_FEATURE_CLIENT: u32 = 2;
/// This build's software decoder loads a GPL build of the machine's codec
/// library as well as an LGPL one (the `gpl-libavcodec` build feature; minor
/// 12). A build without this bit refuses a GPL build with
/// `LOWLAT_ERR_NO_DECODER_LICENCE`. The bit says what the build would load,
/// not what it has: a codec library actually loaded is named with its
/// licence by `lowlat_enum_decoders`.
pub const LOWLAT_FEATURE_GPL_LIBAVCODEC: u32 = 4;

/// Which halves this build of the library carries, and what else was
/// decided when it was built.
///
/// **Asked rather than probed.** A loader that resolves entry points by name
/// would otherwise learn that a half is missing one unresolved symbol at a
/// time; this says it once, before anything else is looked up. The header
/// hides the same halves under the same names (`LOWLAT_HOST`, `LOWLAT_CLIENT`),
/// so an application compiled for one build cannot name what it lacks.
///
/// @returns `LOWLAT_FEATURE_*` bits, OR-ed.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_features() -> u32 {
    let mut bits = 0;
    if cfg!(feature = "host") {
        bits |= LOWLAT_FEATURE_HOST;
    }
    if cfg!(feature = "client") {
        bits |= LOWLAT_FEATURE_CLIENT;
    }
    if cfg!(feature = "gpl-libavcodec") {
        bits |= LOWLAT_FEATURE_GPL_LIBAVCODEC;
    }
    bits
}

/// What each status says about itself.
///
/// A table rather than a match, because the value arriving is an integer and
/// not necessarily one of these.
const DESCRIPTIONS: [(lowlat_status, &CStr); 24] = [
    (LOWLAT_OK, c"ok"),
    (LOWLAT_TIMEOUT, c"no event within the timeout"),
    (
        LOWLAT_ERR_INTERNAL,
        c"a fault was contained at the boundary",
    ),
    (LOWLAT_ERR_INVALID_ARGUMENT, c"an argument was not usable"),
    (LOWLAT_ERR_TOO_SMALL, c"the buffer was too small"),
    (LOWLAT_ERR_POISONED, c"the handle is poisoned"),
    (
        LOWLAT_ERR_ALREADY_STARTED,
        c"this handle is already hosting",
    ),
    (LOWLAT_ERR_NOT_STARTED, c"this handle is not hosting"),
    (
        LOWLAT_ERR_TOO_MANY_HELD,
        c"as many pictures are held as may be",
    ),
    (LOWLAT_ERR_AT_CAPACITY, c"every seat is taken"),
    (
        LOWLAT_ERR_UNKNOWN_ATTEMPT,
        c"no attempt with that identifier",
    ),
    (
        LOWLAT_ERR_ALREADY_BEGUN,
        c"the attempt was already approved",
    ),
    (
        LOWLAT_ERR_WITHDRAWN,
        c"the attempt was withdrawn before it was registered",
    ),
    (LOWLAT_ERR_IO, c"a socket or thread could not be created"),
    (LOWLAT_ERR_CRYPTO, c"credentials could not be produced"),
    (LOWLAT_ERR_UNKNOWN_GUEST, c"no guest with that number"),
    (
        LOWLAT_ERR_FINGERPRINT,
        c"the offer's certificate digest is missing or malformed",
    ),
    (LOWLAT_ERR_NO_DISPLAY, c"nothing is lit"),
    (
        LOWLAT_ERR_DISPLAY_UNREACHABLE,
        c"a display is lit and its framebuffer cannot be reached",
    ),
    (
        LOWLAT_ERR_NO_DECODER_RUNTIME,
        c"the decoder's runtime library is not on the machine",
    ),
    (
        LOWLAT_ERR_NO_DECODER_DEVICE,
        c"no render node opened for the decoder",
    ),
    (
        LOWLAT_ERR_NO_DECODER_PROFILE,
        c"the device decodes none of the profiles a stream could use",
    ),
    (
        LOWLAT_ERR_DECODER_UNSUPPORTED,
        c"the decoder or frame kind asked for is not in this build",
    ),
    (
        LOWLAT_ERR_NO_DECODER_LICENCE,
        c"a codec library was found and is not one this library may load",
    ),
];

/// Describe a status.
///
/// **It takes a plain integer rather than the enumeration**, so that a value
/// from anywhere can be described -- including one this version of the library
/// does not define, which is exactly the case an application reaches for this
/// in. Passing a status to it is an ordinary widening conversion.
///
/// The pointer is to storage that outlives the library, so it is never freed
/// and never copied out of.
///
/// @param[in] status Any status value, including one this version does not define.
/// @returns A NUL-terminated description. Never null, never freed.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_status_string(status: i32) -> *const c_char {
    let text: &CStr = DESCRIPTIONS
        .iter()
        .find(|(code, _)| *code as i32 == status)
        .map_or(c"unknown status", |(_, text)| text);
    text.as_ptr()
}

/// How severe a log line is.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_log_level {
    LOWLAT_LOG_ERROR = 0,
    LOWLAT_LOG_WARN = 1,
    LOWLAT_LOG_INFO = 2,
    LOWLAT_LOG_DEBUG = 3,
    LOWLAT_LOG_TRACE = 4,
}

/// Where log lines go.
///
/// **The one place this library calls into an application**, and the single
/// exception to being poll-based. It is cold, it fires on whichever thread
/// logged, and it must not call back in.
pub type lowlat_log_fn =
    Option<unsafe extern "C" fn(level: u32, message: *const c_char, opaque: *mut c_void)>;

/// The callback and whatever the application wanted handed back with it.
///
/// **A lock rather than an atomic pair**, because the two must be read
/// together: a callback taken with the previous registration's opaque pointer
/// would hand an application a pointer belonging to something it has already
/// forgotten. Logging is cold enough to afford it.
static LOGGER: std::sync::Mutex<(lowlat_log_fn, usize)> = std::sync::Mutex::new((None, 0));

/// Hand one already-formatted line to whatever the application registered.
///
/// **Installed once and replaceable behind that**, so an application may
/// change where its logs go without the underlying sink -- which is
/// process-wide and takes one installation -- having to be changed with it.
fn to_application(level: lowlat_common::log::Level, message: &str) {
    let Ok(logger) = LOGGER.lock() else {
        return;
    };
    let (Some(callback), opaque) = *logger else {
        return;
    };
    // **A copy, because a Rust string has no terminator and C reads one.**
    // Logging allocates here and nowhere else on this path; a line with an
    // interior NUL is truncated at it rather than dropped, since a short
    // message beats a lost one.
    let Ok(text) = std::ffi::CString::new(message) else {
        let Ok(truncated) = std::ffi::CString::new(
            message
                .split('\0')
                .next()
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
        ) else {
            return;
        };
        unsafe { callback(level as u32, truncated.as_ptr(), opaque as *mut c_void) };
        return;
    };
    unsafe { callback(level as u32, text.as_ptr(), opaque as *mut c_void) };
}

/// Receive log messages from every part of this library.
///
/// Passing `NULL` stops delivery and returns the library to writing lines on
/// standard error itself.
///
/// **The callback may be replaced.** The underlying sink is process-wide and
/// installed once; what an application registers here sits behind it, so
/// calling this again changes where lines go rather than being refused.
///
/// @param[in] fn_ Where lines go, or `NULL` to return them to standard error.
/// @param[in] opaque Handed back to `fn_` untouched.
/// @returns [`LOWLAT_OK`].
///
/// # Safety
///
/// `fn_` must remain callable, and `opaque` valid, until this is called
/// again with something else or with `NULL`. It may fire on any thread, and it must not
/// call back into this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_set_log_callback(
    fn_: lowlat_log_fn,
    opaque: *mut c_void,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        {
            let Ok(mut logger) = LOGGER.lock() else {
                return LOWLAT_ERR_INTERNAL;
            };
            *logger = (fn_, opaque as usize);
        }
        // Installed on the first registration and never again; a later one
        // only changes what the shim finds.
        lowlat_common::log::set_sink(to_application);
        LOWLAT_OK
    })
}

/// Set how much is logged. Lines above this level are not formatted at all.
///
/// @param[in] level One of [`lowlat_log_level`].
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_INVALID_ARGUMENT`] for a level nothing
/// defines.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_set_log_level(level: u32) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        let level = match level {
            code if code == lowlat_log_level::LOWLAT_LOG_ERROR as u32 => {
                lowlat_common::log::Level::Error
            }
            code if code == lowlat_log_level::LOWLAT_LOG_WARN as u32 => {
                lowlat_common::log::Level::Warn
            }
            code if code == lowlat_log_level::LOWLAT_LOG_INFO as u32 => {
                lowlat_common::log::Level::Info
            }
            code if code == lowlat_log_level::LOWLAT_LOG_DEBUG as u32 => {
                lowlat_common::log::Level::Debug
            }
            code if code == lowlat_log_level::LOWLAT_LOG_TRACE as u32 => {
                lowlat_common::log::Level::Trace
            }
            _ => return LOWLAT_ERR_INVALID_ARGUMENT,
        };
        lowlat_common::log::set_level(level);
        LOWLAT_OK
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

    /// **The features say what this build decided**, each bit the build
    /// feature of the same name: the halves, and whether the software
    /// decoder loads a GPL codec library.
    #[test]
    fn the_features_are_the_builds_own() {
        let bits = lowlat_features();
        assert_eq!(bits & LOWLAT_FEATURE_HOST != 0, cfg!(feature = "host"));
        assert_eq!(bits & LOWLAT_FEATURE_CLIENT != 0, cfg!(feature = "client"));
        assert_eq!(
            bits & LOWLAT_FEATURE_GPL_LIBAVCODEC != 0,
            cfg!(feature = "gpl-libavcodec")
        );
        assert_eq!(
            bits & !(LOWLAT_FEATURE_HOST | LOWLAT_FEATURE_CLIENT | LOWLAT_FEATURE_GPL_LIBAVCODEC),
            0,
            "a bit no feature names"
        );
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
            LOWLAT_ERR_ALREADY_STARTED,
            LOWLAT_ERR_NOT_STARTED,
            LOWLAT_ERR_AT_CAPACITY,
            LOWLAT_ERR_UNKNOWN_ATTEMPT,
            LOWLAT_ERR_ALREADY_BEGUN,
            LOWLAT_ERR_WITHDRAWN,
            LOWLAT_ERR_IO,
            LOWLAT_ERR_CRYPTO,
            LOWLAT_ERR_UNKNOWN_GUEST,
            LOWLAT_ERR_NO_DISPLAY,
            LOWLAT_ERR_DISPLAY_UNREACHABLE,
        ] {
            let text = lowlat_status_string(status as i32);
            assert!(!text.is_null());
            // Safe: the pointer is to a literal with static storage.
            let text = unsafe { CStr::from_ptr(text) };
            assert_ne!(
                text.to_bytes(),
                b"unknown status",
                "{status:?} is missing from the description table"
            );
        }
        let unknown = unsafe { CStr::from_ptr(lowlat_status_string(-31337)) };
        assert_eq!(unknown.to_bytes(), b"unknown status");
    }
}

#[cfg(test)]
mod logging_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// **Only this test's own lines are counted.** The sink is process-wide
    /// and every other test in this binary logs into it, so counting
    /// everything would make this pass or fail on what else happened to be
    /// running.
    const MARK: &str = "logtest:";

    static SEEN: AtomicU32 = AtomicU32::new(0);
    static LEVEL_SEEN: AtomicU32 = AtomicU32::new(99);
    static TERMINATED: AtomicU32 = AtomicU32::new(0);
    static OPAQUE_KEPT: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn counted(level: u32, message: *const c_char, opaque: *mut c_void) {
        if message.is_null() {
            return;
        }
        // **Read as a C string, which is the whole reason for the copy.** A
        // message that was not terminated would run off the end here rather
        // than parse, so reaching this at all is half the assertion.
        let text = unsafe { CStr::from_ptr(message) };
        let Ok(text) = text.to_str() else {
            return;
        };
        if !text.starts_with(MARK) {
            return;
        }
        TERMINATED.fetch_add(1, Ordering::Relaxed);
        if opaque as usize == 0x1234 {
            OPAQUE_KEPT.fetch_add(1, Ordering::Relaxed);
        }
        LEVEL_SEEN.store(level, Ordering::Relaxed);
        SEEN.fetch_add(1, Ordering::Relaxed);
    }

    /// Forward to standard error, so clearing the callback at the end of this
    /// test does not silence every test that runs after it.
    unsafe extern "C" fn to_stderr(level: u32, message: *const c_char, _opaque: *mut c_void) {
        if message.is_null() {
            return;
        }
        let text = unsafe { CStr::from_ptr(message) };
        eprintln!("[{level}] {}", text.to_string_lossy());
    }

    /// **The line reaches the application terminated, with its own pointer
    /// handed back**, and the level still decides what is formatted at all.
    #[test]
    fn a_registered_callback_receives_lines_and_its_own_pointer() {
        assert_eq!(
            unsafe { lowlat_set_log_callback(Some(counted), 0x1234 as *mut c_void) },
            LOWLAT_OK
        );
        SEEN.store(0, Ordering::Relaxed);
        lowlat_common::log_warn!("{MARK} a line, key=value");
        assert_eq!(SEEN.load(Ordering::Relaxed), 1);
        assert_eq!(TERMINATED.load(Ordering::Relaxed), 1);
        assert_eq!(
            OPAQUE_KEPT.load(Ordering::Relaxed),
            1,
            "the opaque pointer did not survive the round trip"
        );
        assert_eq!(
            LEVEL_SEEN.load(Ordering::Relaxed),
            lowlat_log_level::LOWLAT_LOG_WARN as u32
        );

        // **Replaceable**, which the sink underneath is not: clearing stops
        // delivery rather than being refused because something is installed.
        assert_eq!(
            unsafe { lowlat_set_log_callback(None, core::ptr::null_mut()) },
            LOWLAT_OK
        );
        lowlat_common::log_warn!("{MARK} after clearing");
        assert_eq!(
            SEEN.load(Ordering::Relaxed),
            1,
            "a line arrived after the callback was cleared"
        );

        // A level nothing defines is refused rather than quietly clamped.
        assert_eq!(lowlat_set_log_level(99), LOWLAT_ERR_INVALID_ARGUMENT);
        assert_eq!(
            lowlat_set_log_level(lowlat_log_level::LOWLAT_LOG_INFO as u32),
            LOWLAT_OK
        );

        unsafe { lowlat_set_log_callback(Some(to_stderr), core::ptr::null_mut()) };
    }
}
