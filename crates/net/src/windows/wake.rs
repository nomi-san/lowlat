//! The application send wake, as an entry posted to the loop's completion
//! port.
//!
//! **The port is the loop's only wait**, so the wake has to arrive through it:
//! a producer posts an entry that the wait dequeues beside the receives. The
//! port is made here rather than with the receive storage, because a wake
//! exists before the socket it will share a loop with.
//!
//! **A post is not a counter.** Every post is its own entry, so the collapse
//! an eventfd gives for free is an armed flag here: the first notify after a
//! take posts, the rest find the flag set and return, and `take` clears it.
//! A take that finds it set also covers whatever the producers enqueued
//! before notifying, which is the same ordering argument the portable module
//! makes.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, PostQueuedCompletionStatus};

use super::KEY_WAKE;

/// The port and the flag, shared by the loop's end and every producer's.
///
/// Shared ownership rather than a handle per producer, as on Linux, because
/// the flag is memory and not a kernel object: a producer's end must keep it
/// alive for as long as the producer may notify. The count moves when a handle
/// is made or dropped, never on a notify.
#[derive(Debug)]
struct Shared {
    /// On its own line: producers on other threads write it on every
    /// notify, and the loop clears it on every take.
    armed: Armed,
    port: HANDLE,
}

#[repr(align(64))]
#[derive(Debug, Default)]
struct Armed(AtomicBool);

// SAFETY: the port handle is a process-wide kernel object usable from any
// thread, and the flag is atomic.
unsafe impl Send for Shared {}
// SAFETY: as above; nothing here is reached other than through the handle's
// thread-safe calls and the atomic.
unsafe impl Sync for Shared {}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the port was created by `Wake::new` and is closed once, here,
        // when the last end lets go of it.
        unsafe { CloseHandle(self.port) };
    }
}

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
    /// Create the port the loop will wait on.
    pub fn new() -> io::Result<Self> {
        // SAFETY: a new port with no handle associated; one thread consumes it.
        let port =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, core::ptr::null_mut(), 0, 1) };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            shared: Arc::new(Shared {
                armed: Armed::default(),
                port,
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

    /// The port, for the receive storage to join and the loop to wait on.
    pub(super) fn port(&self) -> HANDLE {
        self.shared.port
    }
}

impl WakeHandle {
    /// Wake the loop. Safe to call from any thread, at any time.
    pub fn notify(&self) -> io::Result<()> {
        // Only the notify that arms the flag posts. One already armed means
        // an entry is on its way to the loop or already taken by its wait,
        // and the loop has not yet consumed it; either way it will look.
        if self.shared.armed.0.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        // SAFETY: the port is open while any end holds it; the entry carries
        // no overlapped structure, only the key.
        let ok =
            unsafe { PostQueuedCompletionStatus(self.shared.port, 0, KEY_WAKE, core::ptr::null()) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
