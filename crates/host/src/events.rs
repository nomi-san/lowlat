//! The queue an application takes events off.
//!
//! **It is deliberately not part of the seam's own state.** Every other call
//! into the seam is a lock acquisition and a copy, and a poll waits for as long
//! as its caller asked; if the two shared a lock, a poll with a a hundred
//! millisecond timeout would stop a hundred milliseconds of everything else
//! ([06 §8](../../../docs/06-api.md)). So the consumer holds this directly and
//! the seam only ever pushes.
//!
//! **Bounded, and dropping the oldest.** An application that stops polling must
//! not be able to grow this without limit, and the loss has to be visible: a
//! count travels with the next event that is delivered, which is the difference
//! between an application that knows it missed something and one that does not.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::admission::Event;
use lowlat_common::clock;

/// How many events are held before the oldest is dropped.
///
/// Sized for a stall rather than for a backlog: an application polling at any
/// sane cadence never approaches this, and one that has stopped is not going to
/// be helped by a larger number.
const MAX_EVENTS: usize = 256;

/// How many bytes of message body are held, across every queued event.
///
/// **A count of events is not a bound.** An application message may carry a
/// megabyte ([`lowlat_core::control::USER_DATA_MAX`]), so a queue limited only
/// by how many events it holds is limited to that many megabytes. Four of them
/// is room for several of the largest bodies anyone actually sends, and a
/// ceiling a stalled consumer cannot walk past.
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// What one event costs against the byte budget.
///
/// Only a message body is worth counting. Everything else is a handful of
/// fixed-size fields, and the queue is bounded by its length as well.
fn weight(event: &Event) -> usize {
    match event {
        Event::UserData { text, .. } => text.len(),
        _ => 0,
    }
}

/// An event, and what was lost before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub event: Event,
    /// How many events were dropped since the previous delivery.
    ///
    /// **Reported on the next event rather than on the one that was dropped**,
    /// which is the only place it can be reported: the drop happens because
    /// nobody is listening.
    pub dropped: u32,
}

#[derive(Debug, Default)]
struct State {
    queued: VecDeque<Event>,
    /// The sum of every queued event's weight, kept rather than recomputed
    /// because a push walks it.
    bytes: usize,
    /// Dropped since the last delivery.
    dropped: u32,
}

impl State {
    fn push(&mut self, event: Event) {
        let arriving = weight(&event);
        // **The event being pushed is never the one dropped.** A body larger
        // than the whole budget would empty the queue and still not fit, and a
        // bound that discards what it was just handed loses the message and
        // reports the loss to nobody. So the oldest go until it fits or until
        // there is nothing left to give up.
        while !self.queued.is_empty()
            && (self.queued.len() >= MAX_EVENTS || self.bytes + arriving > MAX_BYTES)
        {
            if let Some(oldest) = self.queued.pop_front() {
                self.bytes -= weight(&oldest);
                self.dropped = self.dropped.saturating_add(1);
            }
        }
        self.bytes += arriving;
        self.queued.push_back(event);
    }

    fn take(&mut self) -> Option<Received> {
        let event = self.queued.pop_front()?;
        self.bytes -= weight(&event);
        Some(Received {
            event,
            dropped: core::mem::take(&mut self.dropped),
        })
    }
}

#[derive(Debug, Default)]
struct Shared {
    /// Bumped on every push, and the address a waiting consumer parks on.
    ///
    /// **It counts arrivals rather than saying "something is there".** A
    /// consumer samples it, finds the queue empty and sleeps against the value
    /// it sampled, so a push landing in between changes the value and the wait
    /// returns at once instead of sleeping through it.
    arrivals: AtomicU32,
    state: Mutex<State>,
}

impl Shared {
    /// **A poisoned lock is not a reason to stop delivering events.** Every
    /// section under it is a push or a pop with no allocation-free invariant to
    /// break halfway, so the state a panicking thread left is the state before
    /// or after its own operation, and either is consistent.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|held| held.into_inner())
    }
}

/// Where events are put. Cloned into every thread that raises one.
#[derive(Debug, Clone)]
pub struct Sender {
    shared: Arc<Shared>,
}

impl Sender {
    pub fn send(&self, event: Event) {
        self.shared.state().push(event);
        // Release pairs with the consumer's acquire: a consumer that sees this
        // bump sees the push that preceded it.
        self.shared.arrivals.fetch_add(1, Ordering::Release);
        lowlat_common::wait::notify_one(&self.shared.arrivals);
    }
}

/// Where events are taken from. **One consumer**, which is what lets the
/// dropped count be reported exactly once.
#[derive(Debug)]
pub struct Receiver {
    shared: Arc<Shared>,
}

impl Receiver {
    /// Take one if there is one. Never blocks.
    pub fn try_recv(&self) -> Option<Received> {
        self.shared.state().take()
    }

    /// Take one, waiting up to `timeout` for it to arrive.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Received> {
        match self.wait_for(timeout, |state| state.take().map(Delivery::Took)) {
            Delivery::Took(received) => Some(received),
            _ => None,
        }
    }

    /// Take one into the caller's own buffer, waiting up to `timeout`.
    ///
    /// **A body that does not fit is not consumed.** The length it needed is
    /// reported and the event stays at the head, so an application running a
    /// small buffer loses nothing by trying: it calls again with more room.
    /// That is what a poll being a peek that commits on delivery buys, and it
    /// is why the alternative -- publish the ceiling and make every caller
    /// carry a megabyte of scratch -- was not taken.
    pub fn recv_timeout_into(&self, timeout: Duration, body: &mut [u8]) -> Delivery {
        self.wait_for(timeout, |state| {
            let needed = weight(state.queued.front()?);
            if needed > body.len() {
                return Some(Delivery::TooSmall { needed });
            }
            let received = state.take()?;
            if let Event::UserData { text, .. } = &received.event {
                body.get_mut(..text.len())?.copy_from_slice(text);
            }
            Some(Delivery::Took(received))
        })
    }

    /// The wait, shared by both ways of taking.
    ///
    /// Spurious wakes are permitted and the predicate is rechecked, which is
    /// also what makes the sampled arrival count correct: it is read before the
    /// queue is found empty, so a push between the two cannot be slept through.
    fn wait_for(
        &self,
        timeout: Duration,
        mut take: impl FnMut(&mut State) -> Option<Delivery>,
    ) -> Delivery {
        let began = clock::Time::now();
        loop {
            // Sampled first, because a push after this point changes it and the
            // wait below then returns immediately.
            let seen = self.shared.arrivals.load(Ordering::Acquire);
            if let Some(delivery) = take(&mut self.shared.state()) {
                return delivery;
            }
            let waited = Duration::from_secs_f64(clock::elapsed_ms(began) / 1000.0);
            let Some(left) = timeout.checked_sub(waited) else {
                return Delivery::Empty;
            };
            if left.is_zero() {
                return Delivery::Empty;
            }
            lowlat_common::wait::wait(&self.shared.arrivals, seen, left);
        }
    }
}

/// What one take produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Nothing arrived before the timeout.
    Empty,
    /// An event, its body already copied into the caller's buffer if it had
    /// one.
    Took(Received),
    /// The head carries more than the buffer offered. **Nothing was
    /// consumed**, so calling again with this much room delivers it.
    TooSmall { needed: usize },
}

/// One queue, as the two ends of it.
pub fn queue() -> (Sender, Receiver) {
    let shared = Arc::new(Shared::default());
    (
        Sender {
            shared: Arc::clone(&shared),
        },
        Receiver { shared },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn user_data(bytes: usize) -> Event {
        Event::UserData {
            guest: 1,
            id: 9,
            text: vec![b'x'; bytes],
        }
    }

    fn ready(attempt: &str) -> Event {
        Event::Ready {
            attempt: attempt.to_string(),
        }
    }

    /// The ordinary case, and the dropped count that goes with it.
    #[test]
    fn an_event_arrives_in_order_and_reports_nothing_lost() {
        let (sender, receiver) = queue();
        sender.send(ready("first"));
        sender.send(ready("second"));

        let taken = receiver.try_recv().expect("the first is there");
        assert_eq!(taken.event, ready("first"));
        assert_eq!(taken.dropped, 0);
        assert_eq!(
            receiver.try_recv().expect("the second").event,
            ready("second")
        );
        assert!(receiver.try_recv().is_none());
    }

    /// **A stalled consumer bounds the queue and is told what it cost.**
    #[test]
    fn the_oldest_are_dropped_and_counted_against_the_next_delivery() {
        let (sender, receiver) = queue();
        for index in 0..MAX_EVENTS + 10 {
            sender.send(ready(&index.to_string()));
        }

        let taken = receiver.try_recv().expect("something survived");
        assert_eq!(
            taken.dropped, 10,
            "ten past the ceiling should cost exactly the ten oldest"
        );
        // What survives is the newest, not the oldest: the ten that went are
        // the ones at the front.
        assert_eq!(taken.event, ready("10"));

        // And the count is reported once, not on every event after it.
        assert_eq!(receiver.try_recv().expect("the next").dropped, 0);
    }

    /// **The byte budget is a second ceiling, and it is the one that matters.**
    /// Four large bodies are nowhere near the event count and are most of the
    /// budget; a queue bounded only by its length would hold every one.
    #[test]
    fn a_few_large_bodies_reach_the_byte_ceiling_long_before_the_event_ceiling() {
        let (sender, receiver) = queue();
        let body = MAX_BYTES / 4;
        for _ in 0..6 {
            sender.send(user_data(body));
        }

        let taken = receiver.try_recv().expect("something survived");
        assert_eq!(
            taken.dropped, 2,
            "six bodies of a quarter of the budget should cost the two oldest"
        );
        let mut held = 1;
        while receiver.try_recv().is_some() {
            held += 1;
        }
        assert_eq!(held, 4, "the queue held more than the byte budget allows");
    }

    /// **What was just handed over is never what gets dropped.** A body past
    /// the whole budget empties the queue and is still delivered, because a
    /// bound that eats the newest loses the message and reports it to nobody.
    #[test]
    fn a_body_larger_than_the_budget_is_still_delivered() {
        let (sender, receiver) = queue();
        sender.send(ready("first"));
        sender.send(user_data(MAX_BYTES + 1));

        let taken = receiver.try_recv().expect("the oversized body arrives");
        assert_eq!(taken.dropped, 1, "the one ahead of it should have gone");
        assert_eq!(weight(&taken.event), MAX_BYTES + 1);
    }

    /// **A poll waits out its timeout and gives up**, rather than returning at
    /// once or waiting forever.
    #[test]
    fn an_empty_queue_waits_for_the_timeout_and_then_gives_up() {
        let (_sender, receiver) = queue();
        let began = clock::Time::now();
        assert!(receiver.recv_timeout(Duration::from_millis(60)).is_none());
        let waited = clock::elapsed_ms(began);
        assert!(
            waited >= 50.0,
            "returned after {waited:.1} ms, so it did not wait"
        );
        assert!(waited < 2000.0, "waited {waited:.1} ms for a 60 ms timeout");
    }

    /// **A zero timeout does not block**, which is how an application polls on
    /// its own cadence.
    #[test]
    fn a_zero_timeout_returns_at_once() {
        let (sender, receiver) = queue();
        let began = clock::Time::now();
        assert!(receiver.recv_timeout(Duration::ZERO).is_none());
        assert!(clock::elapsed_ms(began) < 1000.0);

        sender.send(ready("there"));
        assert!(receiver.recv_timeout(Duration::ZERO).is_some());
    }

    /// **A push wakes the wait**, which is the whole reason the queue is not
    /// polled on a timer. Without the wake this passes only by timing out, so
    /// the timeout is far longer than the sleep before the send.
    #[test]
    fn an_arrival_wakes_a_waiting_consumer() {
        let (sender, receiver) = queue();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            sender.send(ready("late"));
        });

        let began = clock::Time::now();
        let taken = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("the late event arrives");
        let waited = clock::elapsed_ms(began);
        assert_eq!(taken.event, ready("late"));
        assert!(
            waited < 5000.0,
            "took {waited:.1} ms, so the wait was not woken and this timed out"
        );
    }

    /// Every thread that raises an event has its own handle.
    #[test]
    fn many_producers_reach_one_consumer() {
        let (sender, receiver) = queue();
        let threads: Vec<_> = (0..4)
            .map(|index| {
                let sender = sender.clone();
                std::thread::spawn(move || sender.send(ready(&index.to_string())))
            })
            .collect();
        for thread in threads {
            thread.join().expect("the producer finished");
        }

        let mut seen = 0;
        while receiver.try_recv().is_some() {
            seen += 1;
        }
        assert_eq!(seen, 4);
    }

    /// The address form is only here to keep the unused-import warning honest
    /// about what an event can carry.
    #[test]
    fn an_event_carrying_an_address_queues_like_any_other() {
        let (sender, receiver) = queue();
        let addr: SocketAddr = "127.0.0.1:1234".parse().expect("a literal address");
        sender.send(Event::Established {
            attempt: "one".to_string(),
            addr,
        });
        assert!(matches!(
            receiver.try_recv().map(|taken| taken.event),
            Some(Event::Established { .. })
        ));
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    fn user_data(bytes: usize) -> Event {
        Event::UserData {
            guest: 3,
            id: 11,
            text: vec![b'z'; bytes],
        }
    }

    /// The body lands in the caller's buffer and nothing is allocated for it.
    #[test]
    fn a_body_is_copied_into_the_callers_buffer() {
        let (sender, receiver) = queue();
        sender.send(user_data(5));

        let mut buffer = [0u8; 64];
        let delivery = receiver.recv_timeout_into(Duration::ZERO, &mut buffer);
        let Delivery::Took(taken) = delivery else {
            panic!("expected a delivery, got {delivery:?}");
        };
        assert!(matches!(
            taken.event,
            Event::UserData {
                guest: 3,
                id: 11,
                ..
            }
        ));
        assert_eq!(&buffer[..5], b"zzzzz");
    }

    /// **Too small does not consume it.** The message survives a caller that
    /// guessed its buffer size wrong, which is the whole reason a poll peeks
    /// before it commits.
    #[test]
    fn a_body_that_does_not_fit_is_left_where_it_was() {
        let (sender, receiver) = queue();
        sender.send(user_data(100));

        let mut small = [0u8; 8];
        assert_eq!(
            receiver.recv_timeout_into(Duration::ZERO, &mut small),
            Delivery::TooSmall { needed: 100 },
            "a body that did not fit should say how much room it wanted"
        );

        // And it is still there, undamaged, for a caller that brought more.
        let mut room = [0u8; 128];
        let Delivery::Took(taken) = receiver.recv_timeout_into(Duration::ZERO, &mut room) else {
            panic!("the message was consumed by the attempt that could not take it");
        };
        assert_eq!(taken.dropped, 0);
        assert_eq!(&room[..100], &[b'z'; 100]);
    }

    /// An event with no body needs no room at all, so a caller that never
    /// expects one can offer nothing.
    #[test]
    fn an_event_without_a_body_needs_no_buffer() {
        let (sender, receiver) = queue();
        sender.send(Event::Ready {
            attempt: "a".to_string(),
        });
        assert!(matches!(
            receiver.recv_timeout_into(Duration::ZERO, &mut []),
            Delivery::Took(_)
        ));
    }

    /// An empty queue reports empty rather than a zero-length body, which a
    /// caller would otherwise read as a message that said nothing.
    #[test]
    fn an_empty_queue_is_told_apart_from_an_empty_body() {
        let (sender, receiver) = queue();
        let mut buffer = [0u8; 16];
        assert_eq!(
            receiver.recv_timeout_into(Duration::ZERO, &mut buffer),
            Delivery::Empty
        );

        sender.send(user_data(0));
        assert!(matches!(
            receiver.recv_timeout_into(Duration::ZERO, &mut buffer),
            Delivery::Took(_)
        ));
    }
}
