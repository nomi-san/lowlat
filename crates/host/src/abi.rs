//! The public C ABI.
//!
//! The only public surface there is ([06-api.md](../../../docs/06-api.md)).
//! Naming follows the header rather than Rust convention, which is permitted
//! here and nowhere else.

#![allow(non_camel_case_types)]

use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use crate::admission::{Event, Outcome};
use crate::events::Delivery;
use lowlat_event_type::*;
use lowlat_outcome::*;
use lowlat_status::*;

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
/// ```
///
/// A value is assigned once and never reused, including for a condition that
/// is removed.
#[repr(i32)]
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
}

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

/// What each status says about itself.
///
/// A table rather than a match, because the value arriving is an integer and
/// not necessarily one of these.
const DESCRIPTIONS: [(lowlat_status, &CStr); 6] = [
    (LOWLAT_OK, c"ok"),
    (LOWLAT_TIMEOUT, c"no event within the timeout"),
    (
        LOWLAT_ERR_INTERNAL,
        c"a fault was contained at the boundary",
    ),
    (LOWLAT_ERR_INVALID_ARGUMENT, c"an argument was not usable"),
    (LOWLAT_ERR_TOO_SMALL, c"the buffer was too small"),
    (LOWLAT_ERR_POISONED, c"the handle is poisoned"),
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
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_status_string(status: i32) -> *const c_char {
    let text: &CStr = DESCRIPTIONS
        .iter()
        .find(|(code, _)| *code as i32 == status)
        .map_or(c"unknown status", |(_, text)| text);
    text.as_ptr()
}

/// The longest attempt identifier carried across this boundary.
///
/// **A fixed array rather than a pointer**, because nothing crosses here that
/// the application has to free (docs/06-api.md 10). An identifier longer than
/// this is the application's own, so it is truncated on the way out rather
/// than refused: the event still says what happened, and the application
/// already holds the identifier it made up.
pub const LOWLAT_ATTEMPT_MAX: usize = 128;

/// The longest textual address, which is what an address for a peer's
/// signaling to forward has to be anyway.
pub const LOWLAT_ADDRESS_MAX: usize = 46;

/// Which member of an event is the valid one.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_event_type {
    /// A local candidate, to be sent to the peer as it is found.
    LOWLAT_EVENT_CANDIDATE = 1,
    /// Send the peer a candidate marked ready, once.
    LOWLAT_EVENT_READY = 2,
    /// Connectivity completed and media can flow.
    LOWLAT_EVENT_ESTABLISHED = 3,
    /// The attempt is over, with a reason.
    LOWLAT_EVENT_ENDED = 4,
    /// A guest sent its application a message.
    LOWLAT_EVENT_USER_DATA = 5,
}

/// Why an attempt finished.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_outcome {
    /// Negotiated, and no path was found.
    LOWLAT_OUTCOME_CONNECTIVITY_FAILED = 1,
    /// The peer stopped answering.
    LOWLAT_OUTCOME_PEER_GONE = 2,
    /// Nothing sent has been acknowledged for the delivery deadline, while
    /// something was outstanding the whole time.
    LOWLAT_OUTCOME_UNDELIVERABLE = 3,
    /// The peer said it was leaving.
    LOWLAT_OUTCOME_PEER_LEFT = 4,
    /// Connected, then never said what it could decode.
    LOWLAT_OUTCOME_NEVER_DECLARED = 5,
    /// The socket could not be driven any further.
    LOWLAT_OUTCOME_TRANSPORT_FAILED = 6,
    /// The control stream could not be read any further.
    LOWLAT_OUTCOME_CONTROL_STALLED = 7,
    /// The host ended it, and `reason` carries what the peer was told.
    LOWLAT_OUTCOME_KICKED = 8,
}

/// A local candidate for the application to forward.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_candidate_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
    pub address: [c_char; LOWLAT_ADDRESS_MAX],
    pub port: u16,
    /// Non-zero if a reflexive server reported this one.
    pub from_stun: u8,
    pub reserved: u8,
}

/// Tell the peer this host is ready to be checked.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_ready_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
}

/// A path was found and media is flowing.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_established_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
    pub address: [c_char; LOWLAT_ADDRESS_MAX],
    pub port: u16,
    pub reserved: [u8; 2],
}

/// The attempt is over.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_ended_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
    pub outcome: lowlat_outcome,
    /// What the peer was told, when the outcome is that the host ended it.
    /// Zero otherwise, and zero is not a status a peer stops on.
    pub reason: i32,
}

/// An application message from a guest.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_user_data_event {
    pub guest: u32,
    /// The sub-identifier, which means whatever the application and its
    /// clients agreed it means. Nothing here reads it.
    pub id: u32,
    /// How long the body is. **Not how much was written**: a caller that
    /// offered no buffer is still told what it chose not to receive.
    pub body_len: u32,
}

/// Whichever event this is.
///
/// A union cannot describe itself, and the tag beside it is what says which
/// member to read.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(missing_debug_implementations)]
pub union lowlat_event_body {
    pub candidate: lowlat_candidate_event,
    pub ready: lowlat_ready_event,
    pub established: lowlat_established_event,
    pub ended: lowlat_ended_event,
    pub user_data: lowlat_user_data_event,
}

/// One event.
///
/// **The tag is first** so an application that does not recognise a type can
/// skip it without knowing anything about the rest, which is what makes adding
/// a type additive.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(missing_debug_implementations)]
pub struct lowlat_event {
    pub kind: lowlat_event_type,
    /// How many events were dropped since the previous delivery.
    ///
    /// **Carried on the next event rather than reported at the time**, which
    /// is the only place it can be: the drop happened because nobody was
    /// polling.
    pub dropped: u32,
    pub body: lowlat_event_body,
}

/// What a handle is created with.
///
/// **The caller sets `size`.** It is read rather than assumed, so this can grow
/// without breaking an application compiled against an older header
/// (docs/06-api.md 1).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_create_info {
    pub size: u32,
}

/// One host session, as the application holds it.
///
/// Opaque: the application holds a pointer it cannot look inside, so what is
/// in here changes freely.
#[derive(Debug)]
pub struct lowlat {
    /// Set when a call was contained, and never cleared.
    poisoned: AtomicBool,
    /// **Held directly rather than behind the seam's lock**, because a poll
    /// waits for as long as its caller asked and every other call must stay
    /// answerable while it does. `None` until hosting starts, which is not an
    /// error: an application polls from its own thread before and after.
    events: Option<crate::events::Receiver>,
}

/// Create a handle.
///
/// # Safety
///
/// `out` must point to storage for one pointer. `info` may be null, which
/// takes every default.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_create(
    info: *const lowlat_create_info,
    out: *mut *mut lowlat,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        if out.is_null() {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        // Read only as much as the caller says it wrote. There is nothing
        // beyond the length yet; the check is here because the first version
        // is where the habit is either established or lost.
        if let Some(info) = (unsafe { info.as_ref() })
            && (info.size as usize) < core::mem::size_of::<u32>()
        {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let handle = Box::new(lowlat {
            poisoned: AtomicBool::new(false),
            events: None,
        });
        unsafe { out.write(Box::into_raw(handle)) };
        LOWLAT_OK
    })
}

/// Destroy a handle.
///
/// **Works on a poisoned handle**, which is the point of poisoning: everything
/// else is refused and this still releases what was taken.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`] and is not used again. A null pointer is
/// accepted and does nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_destroy(ll: *mut lowlat) {
    guard((), || {
        if ll.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(ll) });
    });
}

/// Take one event, waiting up to `timeout_ms` for one to arrive.
///
/// Answers [`LOWLAT_TIMEOUT`] when nothing arrived, which is not an error. A
/// `timeout_ms` of zero polls without waiting.
///
/// `body` receives an application message's body and may be null, which means
/// the application does not want bodies: one that arrives is delivered without
/// it, and the event still says how long it was. When `body` is not null,
/// `body_len` carries its capacity in and the bytes written out.
///
/// **A body that does not fit consumes nothing.** [`LOWLAT_ERR_TOO_SMALL`] is
/// answered, `body_len` is set to what the body needs, and the same event is
/// delivered by the next call with room for it.
///
/// # Safety
///
/// `out` must point to one [`lowlat_event`]. `body`, when not null, must point
/// to at least `*body_len` bytes, and `body_len` must then be readable and
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_poll_events(
    ll: *mut lowlat,
    timeout_ms: u32,
    out: *mut lowlat_event,
    body: *mut c_void,
    body_len: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            if out.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            if !body.is_null() && body_len.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let timeout = Duration::from_millis(u64::from(timeout_ms));
            // Nothing is raising events yet, which is a quiet queue rather
            // than a fault. **The timeout is still owed**: an application
            // starts its polling thread before it starts hosting, and a poll
            // that answers instantly turns that thread into a spin. Found by
            // the C harness, which timed the call; the first unit test for
            // this asked for no timeout at all and could not have seen it.
            let Some(events) = handle.events.as_ref() else {
                std::thread::sleep(timeout);
                return LOWLAT_TIMEOUT;
            };

            if body.is_null() {
                // The body is dropped, and the event still reports its length
                // so that what was given up is visible.
                let Some(received) = events.recv_timeout(timeout) else {
                    return LOWLAT_TIMEOUT;
                };
                out.write(described(&received));
                return LOWLAT_OK;
            }

            let capacity = body_len.read() as usize;
            let buffer = core::slice::from_raw_parts_mut(body.cast::<u8>(), capacity);
            match events.recv_timeout_into(timeout, buffer) {
                Delivery::Empty => LOWLAT_TIMEOUT,
                Delivery::TooSmall { needed } => {
                    body_len.write(u32::try_from(needed).unwrap_or(u32::MAX));
                    LOWLAT_ERR_TOO_SMALL
                }
                Delivery::Took(received) => {
                    let event = described(&received);
                    let written = if event.kind == LOWLAT_EVENT_USER_DATA {
                        event.body.user_data.body_len
                    } else {
                        0
                    };
                    body_len.write(written);
                    out.write(event);
                    LOWLAT_OK
                }
            }
        })
    }
}

/// Panic on purpose, and prove the boundary contains it.
///
/// **Exported by the shipped library rather than hidden behind a build
/// option**, because what has to be tested is that *this* object still
/// unwinds. Building it to abort on panic silently disables containment
/// everywhere, and the same code linked into a test binary answers for the
/// test's build rather than for this one.
/// **It takes the handle** so that what follows a contained panic is testable
/// too: the handle is poisoned, every later call on it is refused, and
/// destroying it still works.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_debug_panic(ll: *mut lowlat) -> lowlat_status {
    unsafe {
        entered(ll, |_| {
            panic!("deliberate panic, to prove the boundary contains one")
        })
    }
}

/// Copy text into a fixed array, terminated, truncating if it must.
fn put(into: &mut [c_char], text: &str) {
    into.fill(0);
    let room = into.len().saturating_sub(1);
    for (slot, byte) in into.iter_mut().zip(text.as_bytes().iter().take(room)) {
        *slot = *byte as c_char;
    }
}

/// Say the address and port of a socket into an event's fields.
fn put_address(into: &mut [c_char], port: &mut u16, addr: &std::net::SocketAddr) {
    put(into, &addr.ip().to_string());
    *port = addr.port();
}

/// Describe one event in the shape the boundary publishes.
fn described(received: &crate::events::Received) -> lowlat_event {
    let dropped = received.dropped;
    match &received.event {
        Event::Candidate {
            attempt,
            addr,
            from_stun,
        } => {
            let mut body = lowlat_candidate_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
                address: [0; LOWLAT_ADDRESS_MAX],
                port: 0,
                from_stun: u8::from(*from_stun),
                reserved: 0,
            };
            put(&mut body.attempt, attempt);
            put_address(&mut body.address, &mut body.port, addr);
            lowlat_event {
                kind: LOWLAT_EVENT_CANDIDATE,
                dropped,
                body: lowlat_event_body { candidate: body },
            }
        }
        Event::Ready { attempt } => {
            let mut body = lowlat_ready_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
            };
            put(&mut body.attempt, attempt);
            lowlat_event {
                kind: LOWLAT_EVENT_READY,
                dropped,
                body: lowlat_event_body { ready: body },
            }
        }
        Event::Established { attempt, addr } => {
            let mut body = lowlat_established_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
                address: [0; LOWLAT_ADDRESS_MAX],
                port: 0,
                reserved: [0; 2],
            };
            put(&mut body.attempt, attempt);
            put_address(&mut body.address, &mut body.port, addr);
            lowlat_event {
                kind: LOWLAT_EVENT_ESTABLISHED,
                dropped,
                body: lowlat_event_body { established: body },
            }
        }
        Event::Ended { attempt, outcome } => {
            // Exhaustive on purpose: a new way for an attempt to finish should
            // break this build rather than reach an application as a number it
            // has no name for.
            let (outcome, reason) = match outcome {
                Outcome::ConnectivityFailed => (LOWLAT_OUTCOME_CONNECTIVITY_FAILED, 0),
                Outcome::PeerGone => (LOWLAT_OUTCOME_PEER_GONE, 0),
                Outcome::Undeliverable => (LOWLAT_OUTCOME_UNDELIVERABLE, 0),
                Outcome::PeerLeft => (LOWLAT_OUTCOME_PEER_LEFT, 0),
                Outcome::NeverDeclared => (LOWLAT_OUTCOME_NEVER_DECLARED, 0),
                Outcome::TransportFailed => (LOWLAT_OUTCOME_TRANSPORT_FAILED, 0),
                Outcome::ControlStalled => (LOWLAT_OUTCOME_CONTROL_STALLED, 0),
                Outcome::Kicked(reason) => (LOWLAT_OUTCOME_KICKED, *reason),
            };
            let mut body = lowlat_ended_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
                outcome,
                reason,
            };
            put(&mut body.attempt, attempt);
            lowlat_event {
                kind: LOWLAT_EVENT_ENDED,
                dropped,
                body: lowlat_event_body { ended: body },
            }
        }
        Event::UserData { guest, id, text } => lowlat_event {
            kind: LOWLAT_EVENT_USER_DATA,
            dropped,
            body: lowlat_event_body {
                user_data: lowlat_user_data_event {
                    guest: *guest,
                    id: *id,
                    body_len: u32::try_from(text.len()).unwrap_or(u32::MAX),
                },
            },
        },
    }
}

/// Run an entry point that needs the handle.
///
/// Null is refused, a poisoned handle is refused, and a panic poisons it. **The
/// poisoning is what makes asserting unwind safety sound**: state a panic may
/// have left half-written is never read again, because every later call stops
/// here.
///
/// # Safety
///
/// `ll` is null or came from [`lowlat_create`].
unsafe fn entered(ll: *mut lowlat, call: impl FnOnce(&lowlat) -> lowlat_status) -> lowlat_status {
    let Some(handle) = (unsafe { ll.as_ref() }) else {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    };
    if handle.poisoned.load(Ordering::Acquire) {
        return LOWLAT_ERR_POISONED;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(handle))) {
        Ok(status) => status,
        Err(_) => {
            handle.poisoned.store(true, Ordering::Release);
            lowlat_common::log_error!("abi: a call panicked, contained and the handle poisoned");
            LOWLAT_ERR_INTERNAL
        }
    }
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

    /// **A panic is contained, and what follows it is refused.** The test
    /// that matters runs against the built shared object, in `tests/abi.rs`;
    /// this one says the handle is poisoned by it and that destroying a
    /// poisoned handle still works, which is the half a C harness cannot see.
    #[test]
    fn a_panic_poisons_the_handle_and_destroying_it_still_works() {
        let mut handle: *mut lowlat = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        assert!(!handle.is_null());

        assert_eq!(unsafe { lowlat_debug_panic(handle) }, LOWLAT_ERR_INTERNAL);
        // Every later call, including another panic, stops at the poison.
        assert_eq!(unsafe { lowlat_debug_panic(handle) }, LOWLAT_ERR_POISONED);
        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        assert_eq!(
            unsafe {
                lowlat_host_poll_events(
                    handle,
                    0,
                    event.as_mut_ptr(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            LOWLAT_ERR_POISONED
        );

        unsafe { lowlat_destroy(handle) };
    }

    /// **A null handle is refused rather than dereferenced**, which is the
    /// first thing any application does by accident.
    #[test]
    fn a_null_handle_is_refused() {
        assert_eq!(
            unsafe { lowlat_debug_panic(core::ptr::null_mut()) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        // And destroying nothing is allowed, so an application's cleanup path
        // needs no branch of its own.
        unsafe { lowlat_destroy(core::ptr::null_mut()) };
    }

    /// **Polling a host that has not started is quiet, not broken** -- and it
    /// still costs the time it was asked for. An application starts its
    /// polling thread before it starts hosting, so a poll that answers
    /// instantly makes that thread a spin.
    #[test]
    fn polling_before_hosting_waits_and_then_times_out() {
        let mut handle: *mut lowlat = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        let began = lowlat_common::clock::Time::now();
        assert_eq!(
            unsafe {
                lowlat_host_poll_events(
                    handle,
                    60,
                    event.as_mut_ptr(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            LOWLAT_TIMEOUT
        );
        let waited = lowlat_common::clock::elapsed_ms(began);
        assert!(waited >= 50.0, "returned after {waited:.1} ms, so it spun");
        unsafe { lowlat_destroy(handle) };
    }
}
