//! The application send wake, as a user event on the loop's kernel queue.
//!
//! **The queue is the loop's only wait**, so the wake has to arrive through it:
//! a producer triggers the user event, which the wait reports beside the
//! socket. The queue is made here rather than with the receive storage,
//! because a wake exists before the socket it will share a loop with.
//!
//! **The event only makes the wait return**: the system clears it when a wait
//! reports it, so whether work was announced is an armed flag, as on Windows.
//! The first notify after a take arms the flag and triggers the event, the
//! rest find it armed and return, and `take` clears it -- a take costs no
//! call, and any number of notifies cost one trigger. A take that finds the
//! flag set covers whatever the producers enqueued before notifying, which is
//! the ordering argument the portable module makes.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::queue::{self, WAKE};

/// The queue and the flag, shared by the loop's end and every producer's.
///
/// Shared ownership rather than a descriptor per producer, as on Linux,
/// because the flag is memory: a producer's end must keep it alive for as long
/// as the producer may notify.
#[derive(Debug)]
struct Shared {
    /// On its own line: producers on other threads write it on every
    /// notify, and the loop clears it on every take.
    armed: Armed,
    queue: OwnedFd,
}

#[repr(align(64))]
#[derive(Debug, Default)]
struct Armed(AtomicBool);

/// The consumer's end: owned by the loop, taken once per pass.
#[derive(Debug)]
pub struct Wake {
    shared: Arc<Shared>,
}

/// A producer's end: one per sending thread.
#[derive(Debug)]
pub struct WakeHandle {
    shared: Arc<Shared>,
}

impl Wake {
    /// Create the queue the loop will wait on, its user event registered.
    pub fn new() -> io::Result<Self> {
        // SAFETY: kqueue takes no argument; the descriptor is handed straight
        // to OwnedFd, which closes it on drop. A queue is not inherited by a
        // child, so it needs no close-on-exec.
        let raw = unsafe { libc::kqueue() };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own and have registered nowhere.
        let queue = unsafe { OwnedFd::from_raw_fd(raw) };
        // Reported once per trigger, then quiet until the next one.
        queue::apply(
            queue.as_raw_fd(),
            &[queue::event(
                WAKE,
                libc::EVFILT_USER,
                libc::EV_ADD | libc::EV_CLEAR,
                libc::NOTE_FFNOP,
                0,
            )],
        )?;
        Ok(Self {
            shared: Arc::new(Shared {
                armed: Armed::default(),
                queue,
            }),
        })
    }

    /// A producer's end, for a thread that will enqueue work.
    pub fn handle(&self) -> io::Result<WakeHandle> {
        Ok(WakeHandle {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Consume the pending wake, if any.
    ///
    /// **Call this before pulling the application rings**, never after. See
    /// the portable module: the reverse order drops the token for an item that
    /// has not been read yet.
    pub fn take(&self) -> io::Result<bool> {
        // AcqRel, pairing with the producer's swap: a take that finds the flag
        // set happens after the notify that set it, and so after everything
        // the producer enqueued before notifying.
        Ok(self.shared.armed.0.swap(false, Ordering::AcqRel))
    }

    /// The queue, for the receive and the relay's stream to join and the
    /// loop to wait on.
    pub(super) fn queue(&self) -> RawFd {
        self.shared.queue.as_raw_fd()
    }
}

impl WakeHandle {
    /// Wake the loop. Safe to call from any thread, at any time.
    pub fn notify(&self) -> io::Result<()> {
        // Only the notify that arms the flag triggers. One already armed means
        // the event is on its way to the loop or already reported to its wait,
        // and the loop has not yet taken it; either way it will look.
        if self.shared.armed.0.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        queue::apply(
            self.shared.queue.as_raw_fd(),
            &[queue::event(
                WAKE,
                libc::EVFILT_USER,
                0,
                libc::NOTE_TRIGGER,
                0,
            )],
        )
    }
}
