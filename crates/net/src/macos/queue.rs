//! The kernel queue's calls, shared by the datagram loop and the relay's
//! stream: a change or an event as the system lays it out, one call that
//! applies changes and collects events, and the pass's deadline.
//!
//! **The deadline is a timer marked critical, never the wait's own timeout.**
//! The system lets every ordinary timed wait -- the queue's own timeout among
//! them -- run late by an eighth to a quarter of its length; a timer marked
//! critical is exempt (docs/02-io-shell.md 2). It is a change in the same call
//! that waits, so it costs no call of its own.

use std::io;
use std::os::fd::RawFd;

use crate::shell::MIN_WAIT_MS;

/// The wake's user event. A socket is its own descriptor under its own
/// filter, and the queue keys on the pair, so no ident here collides with one.
pub(super) const WAKE: usize = 1;

/// The pass's deadline.
const DEADLINE: usize = 2;

/// Events taken per wait: the socket's read and write, the wake and the
/// deadline, with room to spare for a refused change.
pub(super) const EVENTS: usize = 8;

/// One change or event, as the system's `EV_SET` lays it out.
pub(super) const fn event(
    ident: usize,
    filter: i16,
    flags: u16,
    fflags: u32,
    data: isize,
) -> libc::kevent {
    libc::kevent {
        ident,
        filter,
        flags,
        fflags,
        data,
        udata: core::ptr::null_mut(),
    }
}

/// A slot for the system to write an event into.
pub(super) const NONE: libc::kevent = event(0, 0, 0, 0, 0);

/// The deadline `timeout_ms` from now as a change: a one-shot timer marked
/// critical, never shorter than the loop's shortest wait, and rounded up to
/// the microsecond -- rounded down, the pass wakes just before the deadline it
/// armed and pays a second wake to act on it.
pub(super) fn deadline(timeout_ms: f64) -> libc::kevent {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "clamped to the wait bounds by the caller"
    )]
    let micros = (timeout_ms.max(MIN_WAIT_MS) * 1000.0).ceil() as isize;
    event(
        DEADLINE,
        libc::EVFILT_TIMER,
        libc::EV_ADD | libc::EV_ONESHOT,
        libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
        micros.saturating_mul(1000),
    )
}

/// Apply `changes` in one call, waiting for nothing.
pub(super) fn apply(queue: RawFd, changes: &[libc::kevent]) -> io::Result<()> {
    let count = libc::c_int::try_from(changes.len()).map_err(io::Error::other)?;
    // SAFETY: the changes are live for the call and counted exactly; with no
    // event slots and no timeout the call returns once they are applied.
    let rc = unsafe {
        libc::kevent(
            queue,
            changes.as_ptr(),
            count,
            core::ptr::null_mut(),
            0,
            core::ptr::null(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Apply `changes`, then wait for events -- until one arrives, or not at all
/// when `now` -- into the front of `events`, and say how many. An interrupt
/// reports none: whatever was pending is still pending for the next wait.
pub(super) fn wait(
    queue: RawFd,
    changes: &[libc::kevent],
    events: &mut [libc::kevent],
    now: bool,
) -> io::Result<usize> {
    let zero = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let timeout: *const libc::timespec = if now {
        &raw const zero
    } else {
        core::ptr::null()
    };
    let changes_len = libc::c_int::try_from(changes.len()).map_err(io::Error::other)?;
    let events_len = libc::c_int::try_from(events.len()).map_err(io::Error::other)?;
    // SAFETY: both arrays are live for the call and counted exactly; the
    // system writes at most `events_len` events.
    let got = unsafe {
        libc::kevent(
            queue,
            changes.as_ptr(),
            changes_len,
            events.as_mut_ptr(),
            events_len,
            timeout,
        )
    };
    if got < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(0);
        }
        return Err(error);
    }
    let got = usize::try_from(got).unwrap_or(0);
    // A change the system refused comes back as an event marked as an error,
    // carrying the error. The structure is packed: fields are read by value.
    for event in events.get(..got).unwrap_or_default() {
        let (flags, data) = (event.flags, event.data);
        if flags & libc::EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(
                i32::try_from(data).unwrap_or(libc::EINVAL),
            ));
        }
    }
    Ok(got)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The pass's deadline is a critical timer**, read from the change the
    /// wait arms rather than from the clock: no landing time tells it from
    /// the coalesced timeout on every machine -- a shared runner preempts a
    /// critical timer, and a coalesced one lands on time whenever something
    /// else wakes the processor -- so the landings are measured by hand
    /// (`io.rs`), and what is held here is the change.
    #[test]
    fn the_deadline_is_a_critical_timer() {
        let change = deadline(2.5);
        let (filter, flags, fflags, data) =
            (change.filter, change.flags, change.fflags, change.data);
        assert_eq!(filter, libc::EVFILT_TIMER);
        assert_eq!(flags, libc::EV_ADD | libc::EV_ONESHOT);
        assert_eq!(
            fflags,
            libc::NOTE_NSECONDS | libc::NOTE_CRITICAL,
            "a timer not marked critical is coalesced, an eighth to a quarter late"
        );
        assert_eq!(data, 2_500_000);
    }

    /// Never shorter than the shortest wait, and rounded up, never down.
    #[test]
    fn the_deadline_is_rounded_up_and_bounded_below() {
        let (short, fraction) = (deadline(0.2), deadline(1.000_000_4));
        let (short, fraction) = (short.data, fraction.data);
        assert_eq!(short, 1_000_000, "a sub-millisecond wait is a hot poll");
        assert_eq!(
            fraction, 1_001_000,
            "a fractional deadline was rounded down"
        );
    }
}
