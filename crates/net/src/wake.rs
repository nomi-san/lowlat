//! The application send wake.
//!
//! Enqueuing to an application-facing ring must reach the wire in microseconds,
//! not at the next timeout. Without a wake, input on an otherwise idle stream
//! waits out the timer, and input latency is the one budget with a human in the
//! loop.
//!
//! **The ordering is the whole of the correctness argument, and it is easy to
//! get backwards.** The loop takes the wake *before* it pulls the application
//! rings. Then anything enqueued after that point leaves the descriptor armed,
//! so the next wait returns immediately and the work is picked up. Draining the
//! rings first and taking the wake afterwards consumes the token belonging to
//! an item that has not been read yet, and that item sits until the next
//! timeout. It is the same shape as a notify that never reaches its waiter: the
//! wake exists and the sequence around it loses it.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// The consumer's end: owned by the loop, taken once per pass.
#[derive(Debug)]
pub struct Wake {
    fd: OwnedFd,
}

/// A producer's end: one per sending thread, owning its own descriptor.
///
/// Separate descriptors rather than a shared reference count, so ownership
/// stays single and explicit and nothing on the send path touches an atomic
/// refcount.
#[derive(Debug)]
pub struct WakeHandle {
    fd: OwnedFd,
}

impl Wake {
    /// Create the wake descriptor.
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd with a zero initial count and constant flags; the
        // descriptor is handed straight to OwnedFd, which closes it on drop.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own and have not registered elsewhere.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }

    /// A producer's end, for a thread that will enqueue work.
    pub fn handle(&self) -> io::Result<WakeHandle> {
        Ok(WakeHandle {
            fd: self.fd.try_clone()?,
        })
    }

    /// Consume the pending wake, if any.
    ///
    /// **Call this before pulling the application rings**, never after. See the
    /// module note: the reverse order drops the token for an item that has not
    /// been read yet.
    pub fn take(&self) -> io::Result<bool> {
        let mut count: u64 = 0;
        // SAFETY: an eventfd read writes exactly eight bytes into the buffer,
        // which is what is passed.
        let got = unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                core::ptr::addr_of_mut!(count).cast(),
                core::mem::size_of::<u64>(),
            )
        };
        if got < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                // Nothing pending. Not a failure: the loop may have woken for a
                // datagram or a timeout instead.
                io::ErrorKind::WouldBlock => Ok(false),
                _ => Err(error),
            };
        }
        Ok(count > 0)
    }
}

impl WakeHandle {
    /// Wake the loop. Safe to call from any thread, at any time.
    pub fn notify(&self) -> io::Result<()> {
        let count: u64 = 1;
        // SAFETY: an eventfd write reads exactly eight bytes from the buffer,
        // which is what is passed.
        let put = unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                core::ptr::addr_of!(count).cast(),
                core::mem::size_of::<u64>(),
            )
        };
        if put < 0 {
            let error = io::Error::last_os_error();
            // The counter saturating means the loop has not drained yet, which
            // is exactly the state a wake is meant to leave behind. Nothing to
            // report.
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }
}

impl AsRawFd for Wake {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wake_is_taken_once() {
        let wake = Wake::new().expect("wake");
        let producer = wake.handle().expect("handle");

        assert!(!wake.take().expect("take"), "a fresh wake must be idle");
        producer.notify().expect("notify");
        assert!(wake.take().expect("take"), "the wake did not fire");
        assert!(!wake.take().expect("take"), "the wake fired twice");
    }

    /// Several notifications collapse into one pending wake, which is what
    /// makes the wake cheap: producers never queue behind each other.
    #[test]
    fn many_notifications_collapse() {
        let wake = Wake::new().expect("wake");
        let producer = wake.handle().expect("handle");
        for _ in 0..100 {
            producer.notify().expect("notify");
        }
        assert!(wake.take().expect("take"));
        assert!(!wake.take().expect("take"));
    }

    /// The ordering the module exists to enforce. A notification that lands
    /// after the loop has taken the wake must leave it armed, so the next wait
    /// returns at once instead of sitting out the timeout.
    #[test]
    fn a_notification_after_the_take_leaves_the_wake_armed() {
        let wake = Wake::new().expect("wake");
        let producer = wake.handle().expect("handle");

        producer.notify().expect("notify");
        assert!(wake.take().expect("take"));

        // This is the enqueue that races the drain.
        producer.notify().expect("notify");
        assert!(
            wake.take().expect("take"),
            "work enqueued after the take was lost"
        );
    }

    #[test]
    fn a_handle_works_from_another_thread() {
        let wake = Wake::new().expect("wake");
        let producer = wake.handle().expect("handle");
        std::thread::spawn(move || producer.notify().expect("notify"))
            .join()
            .expect("join");
        assert!(wake.take().expect("take"));
    }
}
