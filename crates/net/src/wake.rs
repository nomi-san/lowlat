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
//!
//! The mechanism is the platform's (`crate::sys`); the tests below are the
//! contract every platform's meets.

use std::io;

#[derive(Debug)]
pub struct Wake {
    pub(crate) kind: Kind,
}

#[derive(Debug)]
pub(crate) enum Kind {
    Datagram(crate::sys::Wake),
    #[cfg(feature = "turn-stream")]
    Stream(Box<crate::stream_io::StreamIo>),
}

#[derive(Debug)]
pub struct WakeHandle {
    kind: HandleKind,
}

#[derive(Debug)]
enum HandleKind {
    Datagram(crate::sys::WakeHandle),
    #[cfg(feature = "turn-stream")]
    Stream(std::sync::Arc<crate::stream_io::Notify>),
}

impl Wake {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            kind: Kind::Datagram(crate::sys::Wake::new()?),
        })
    }

    /// Prepare a nonblocking relay carrier. Connection/handshake progress is
    /// driven later by prepare(), on the session thread, not the calling UI.
    #[cfg(feature = "turn-stream")]
    pub fn stream(
        server: std::net::SocketAddr,
        security: crate::stream_io::Security,
    ) -> io::Result<Self> {
        Ok(Self {
            kind: Kind::Stream(Box::new(crate::stream_io::StreamIo::start(
                server, security,
            )?)),
        })
    }

    pub fn handle(&self) -> io::Result<WakeHandle> {
        match &self.kind {
            Kind::Datagram(wake) => Ok(WakeHandle::datagram(wake.handle()?)),
            #[cfg(feature = "turn-stream")]
            Kind::Stream(io) => Ok(WakeHandle::stream(io.notify())),
        }
    }

    pub fn take(&self) -> io::Result<bool> {
        match &self.kind {
            Kind::Datagram(wake) => wake.take(),
            #[cfg(feature = "turn-stream")]
            Kind::Stream(io) => Ok(io.take_wake()),
        }
    }

    /// Finish socket/TLS setup before starting the endpoint's protocol clock.
    /// The same wake interrupts setup on teardown; no helper thread is used.
    pub fn prepare(&mut self, stopping: impl Fn() -> bool) -> io::Result<()> {
        match &mut self.kind {
            Kind::Datagram(_) => {
                let _ = stopping;
                Ok(())
            }
            #[cfg(feature = "turn-stream")]
            Kind::Stream(io) => io.prepare(stopping),
        }
    }
}

impl WakeHandle {
    pub(crate) fn datagram(handle: crate::sys::WakeHandle) -> Self {
        Self {
            kind: HandleKind::Datagram(handle),
        }
    }

    #[cfg(feature = "turn-stream")]
    pub(crate) fn stream(notify: std::sync::Arc<crate::stream_io::Notify>) -> Self {
        Self {
            kind: HandleKind::Stream(notify),
        }
    }

    pub fn notify(&self) -> io::Result<()> {
        match &self.kind {
            HandleKind::Datagram(handle) => handle.notify(),
            #[cfg(feature = "turn-stream")]
            HandleKind::Stream(notify) => notify.notify(),
        }
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
