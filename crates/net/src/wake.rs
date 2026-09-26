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

pub use crate::sys::{Wake, WakeHandle};

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
