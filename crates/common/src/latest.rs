//! A latest-wins ring of slots between one producer and one consumer.
//!
//! The producer fills a slot and publishes it; the consumer takes the newest
//! published slot, holds it for as long as it likes, and gives it back. A
//! consumer that is slow does not slow the producer: when no slot is free the
//! producer overwrites the oldest published slot the consumer has not taken,
//! never one it holds. A picture the consumer has not looked at yet is the
//! one nothing will miss (docs/10-client.md section 4).
//!
//! Two threads move slots through four states, so this is model checked
//! rather than reasoned about, and the check is shown capable of failing.
//!
//! The payload is a `Copy` description the producer writes before it
//! publishes and the consumer reads while it holds; the bytes a slot
//! describes live with the caller, guarded by the same states.

use core::time::Duration;

use crate::sync::{AtomicU32, AtomicU64, Ordering, UnsafeCell};
use crate::wait;

/// The wait word is a real atomic whatever the build: the futex needs its
/// address, and the model check explores the states, not the sleep.
type WaitWord = core::sync::atomic::AtomicU32;

const FREE: u32 = 0;
const FILLING: u32 = 1;
const READY: u32 = 2;
const HELD: u32 = 3;

struct Slot<T> {
    state: AtomicU32,
    /// The publish sequence: what "newer" means.
    seq: AtomicU64,
    payload: UnsafeCell<T>,
}

/// The ring.
pub struct Latest<T: Copy, const N: usize> {
    slots: [Slot<T>; N],
    /// The next sequence the producer stamps. The producer's alone.
    next: AtomicU64,
    /// The consumer's wait word: bumped on every publish and at close.
    word: WaitWord,
    closed: WaitWord,
}

impl<T: Copy + core::fmt::Debug, const N: usize> core::fmt::Debug for Latest<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Latest")
            .field("slots", &N)
            .field("next", &self.next.load(Ordering::Relaxed))
            .finish()
    }
}

// SAFETY: the payload is written only while the slot is FILLING, which one
// producer holds, and read only while it is HELD, which one consumer holds;
// the transitions between them are Release/Acquire pairs on `state`, so the
// two windows never overlap and the write is visible to the read.
unsafe impl<T: Copy + Send, const N: usize> Sync for Latest<T, N> {}
// SAFETY: as above; the payload owns nothing thread-affine.
unsafe impl<T: Copy + Send, const N: usize> Send for Latest<T, N> {}

/// What an acquire handed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Taken<T> {
    pub index: usize,
    pub seq: u64,
    pub payload: T,
}

impl<T: Copy, const N: usize> Latest<T, N> {
    pub fn new(blank: T) -> Self {
        Self {
            slots: core::array::from_fn(|_| Slot {
                state: AtomicU32::new(FREE),
                seq: AtomicU64::new(0),
                payload: UnsafeCell::new(blank),
            }),
            next: AtomicU64::new(1),
            word: WaitWord::new(0),
            closed: WaitWord::new(0),
        }
    }

    /// The producer claims a slot to fill: a free one, else the oldest
    /// published one the consumer has not taken. `None` only when every slot
    /// is held or being filled, which a consumer bound to two holds can
    /// never bring about with four slots.
    pub fn begin(&self) -> Option<usize> {
        loop {
            for (index, slot) in self.slots.iter().enumerate() {
                if slot
                    .state
                    .compare_exchange(FREE, FILLING, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(index);
                }
            }
            // Steal the oldest ready slot. The consumer may take it between
            // the scan and the exchange -- and free an older one as it does
            // -- so a failed exchange starts the whole search again.
            let mut oldest: Option<(usize, u64)> = None;
            for (index, slot) in self.slots.iter().enumerate() {
                if slot.state.load(Ordering::Acquire) == READY {
                    let seq = slot.seq.load(Ordering::Relaxed);
                    if oldest.is_none_or(|(_, s)| seq < s) {
                        oldest = Some((index, seq));
                    }
                }
            }
            let Some((index, _)) = oldest else {
                // Nothing free and nothing ready: either every slot is held
                // or being filled, or the consumer freed one between the two
                // scans. Only the first is an answer.
                let taken = self
                    .slots
                    .iter()
                    .filter(|s| matches!(s.state.load(Ordering::Acquire), HELD | FILLING))
                    .count();
                if taken == N {
                    return None;
                }
                continue;
            };
            let slot = self.slots.get(index)?;
            if slot
                .state
                .compare_exchange(READY, FILLING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Some(index);
            }
        }
    }

    /// Write the payload of a slot being filled. Only the producer, only
    /// between `begin` and `publish`.
    pub fn set(&self, index: usize, payload: T) {
        if let Some(slot) = self.slots.get(index) {
            debug_assert_eq!(slot.state.load(Ordering::Relaxed), FILLING);
            slot.payload.with_mut(|p| {
                // SAFETY: the slot is FILLING, held by this producer alone.
                unsafe { *p = payload };
            });
        }
    }

    /// Publish a filled slot: it becomes the newest.
    pub fn publish(&self, index: usize) {
        let Some(slot) = self.slots.get(index) else {
            return;
        };
        let seq = self.next.fetch_add(1, Ordering::Relaxed);
        slot.seq.store(seq, Ordering::Relaxed);
        // Release: the payload and the bytes it describes are visible to the
        // consumer that acquires this.
        slot.state.store(READY, Ordering::Release);
        self.word
            .fetch_add(1, core::sync::atomic::Ordering::Release);
        wait::notify_one(&self.word);
    }

    /// Give a slot back without publishing it.
    pub fn abandon(&self, index: usize) {
        if let Some(slot) = self.slots.get(index) {
            slot.state.store(FREE, Ordering::Release);
        }
    }

    /// Take the newest published slot with a sequence above `after`, waiting
    /// up to `timeout` for one. Older published slots are freed on the way:
    /// they will never be shown.
    pub fn acquire(&self, after: u64, timeout: Duration) -> Option<Taken<T>> {
        let deadline = crate::clock::Time::now();
        let mut remaining = timeout;
        loop {
            let word = self.word.load(core::sync::atomic::Ordering::Acquire);
            if let Some(taken) = self.take_newest(after) {
                return Some(taken);
            }
            if self.closed.load(core::sync::atomic::Ordering::Acquire) != 0 || remaining.is_zero() {
                return None;
            }
            wait::wait(&self.word, word, remaining);
            let elapsed = crate::clock::elapsed_ms(deadline);
            let total = timeout.as_secs_f64() * 1000.0;
            if elapsed >= total {
                remaining = Duration::ZERO;
            } else {
                remaining = Duration::from_secs_f64((total - elapsed) / 1000.0);
            }
        }
    }

    fn take_newest(&self, after: u64) -> Option<Taken<T>> {
        loop {
            let mut newest: Option<(usize, u64)> = None;
            for (index, slot) in self.slots.iter().enumerate() {
                if slot.state.load(Ordering::Acquire) == READY {
                    let seq = slot.seq.load(Ordering::Relaxed);
                    if seq > after && newest.is_none_or(|(_, s)| seq > s) {
                        newest = Some((index, seq));
                    }
                }
            }
            let (index, seq) = newest?;
            let slot = self.slots.get(index)?;
            // The producer may have stolen it since the scan; then look again.
            if slot
                .state
                .compare_exchange(READY, HELD, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                continue;
            }
            // Everything older that is still ready is discarded: latest wins.
            for other in &self.slots {
                if other.state.load(Ordering::Acquire) == READY
                    && other.seq.load(Ordering::Relaxed) < seq
                {
                    let _ = other.state.compare_exchange(
                        READY,
                        FREE,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    );
                }
            }
            // SAFETY: the slot is HELD by this consumer; the producer wrote
            // the payload before the Release that made it READY.
            let payload = slot.payload.with(|p| unsafe { *p });
            return Some(Taken {
                index,
                seq,
                payload,
            });
        }
    }

    /// The consumer is done with a held slot. False if it was not held.
    pub fn release(&self, index: usize) -> bool {
        self.slots.get(index).is_some_and(|slot| {
            slot.state
                .compare_exchange(HELD, FREE, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        })
    }

    /// Wake every waiter for good: no more pictures are coming.
    pub fn close(&self) {
        self.closed.store(1, core::sync::atomic::Ordering::Release);
        self.word
            .fetch_add(1, core::sync::atomic::Ordering::Release);
        wait::notify_all(&self.word);
    }

    /// Whether `close` was called.
    pub fn closed(&self) -> bool {
        self.closed.load(core::sync::atomic::Ordering::Acquire) != 0
    }

    /// Take waiters again after a close, for the producer that comes next.
    /// **Called with no producer running.** A slot published before the close
    /// and never taken is let go, since it belongs to what closed; a held one
    /// stays held until it is given back.
    pub fn reopen(&self) {
        for slot in &self.slots {
            // The consumer may be taking the same slot: one of the two wins
            // it, and a slot taken is never let go here.
            let _ = slot
                .state
                .compare_exchange(READY, FREE, Ordering::AcqRel, Ordering::Relaxed);
        }
        self.closed.store(0, core::sync::atomic::Ordering::Release);
    }

    /// Published slots not yet taken. For the metrics.
    pub fn ready(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state.load(Ordering::Relaxed) == READY)
            .count()
    }

    /// Slots the consumer holds.
    pub fn held(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state.load(Ordering::Relaxed) == HELD)
            .count()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn the_producer_never_blocks_without_a_consumer() {
        let ring = Latest::<u32, 4>::new(0);
        // Far more publishes than slots, and every one finds a slot: the
        // oldest ready one is stolen.
        for n in 0..100u32 {
            let index = ring.begin().expect("a slot");
            ring.set(index, n);
            ring.publish(index);
        }
        assert_eq!(ring.ready(), 4);
        let taken = ring.acquire(0, Duration::ZERO).expect("the newest");
        assert_eq!(taken.payload, 99);
        // The three older ones were discarded on the way.
        assert_eq!(ring.ready(), 0);
    }

    #[test]
    fn a_held_slot_is_never_overwritten() {
        let ring = Latest::<u32, 4>::new(0);
        let index = ring.begin().unwrap();
        ring.set(index, 7);
        ring.publish(index);
        let held = ring.acquire(0, Duration::ZERO).unwrap();
        for n in 0..100u32 {
            let index = ring.begin().expect("a slot");
            assert_ne!(index, held.index, "the held slot was reused");
            ring.set(index, 100 + n);
            ring.publish(index);
        }
        assert_eq!(ring.held(), 1);
        ring.release(held.index);
        assert_eq!(ring.held(), 0);
    }

    #[test]
    fn two_held_slots_leave_the_producer_two_to_work_with() {
        let ring = Latest::<u32, 4>::new(0);
        let mut last = 0;
        let mut held = Vec::new();
        for n in 0..2u32 {
            let index = ring.begin().unwrap();
            ring.set(index, n);
            ring.publish(index);
            let taken = ring.acquire(last, Duration::ZERO).unwrap();
            last = taken.seq;
            held.push(taken.index);
        }
        for n in 0..50u32 {
            let index = ring.begin().expect("a slot with two held");
            assert!(!held.contains(&index));
            ring.set(index, 10 + n);
            ring.publish(index);
        }
    }

    #[test]
    fn a_burst_hands_out_the_last_and_nothing_older_is_seen_again() {
        let ring = Latest::<u32, 4>::new(0);
        for n in 1..=3u32 {
            let index = ring.begin().unwrap();
            ring.set(index, n);
            ring.publish(index);
        }
        let taken = ring.acquire(0, Duration::ZERO).unwrap();
        assert_eq!(taken.payload, 3);
        assert!(
            ring.acquire(taken.seq, Duration::ZERO).is_none(),
            "an older picture came out after a newer one"
        );
    }

    #[test]
    fn a_closed_ring_wakes_a_waiter_empty_handed() {
        let ring = std::sync::Arc::new(Latest::<u32, 4>::new(0));
        let waiter = {
            let ring = ring.clone();
            std::thread::spawn(move || ring.acquire(0, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(20));
        ring.close();
        assert!(waiter.join().unwrap().is_none());
    }

    #[test]
    fn a_reopened_ring_waits_again_and_forgets_what_the_close_left() {
        let ring = Latest::<u32, 4>::new(0);
        // One picture taken and still held, one published and never taken,
        // then the close.
        let index = ring.begin().unwrap();
        ring.set(index, 6);
        ring.publish(index);
        let held = ring.acquire(0, Duration::ZERO).unwrap();
        let index = ring.begin().unwrap();
        ring.set(index, 7);
        ring.publish(index);
        ring.close();

        ring.reopen();
        assert!(!ring.closed());
        let began = std::time::Instant::now();
        assert!(
            ring.acquire(0, Duration::from_millis(50)).is_none(),
            "what the close left was handed out after the reopen"
        );
        assert!(
            began.elapsed() >= Duration::from_millis(40),
            "a reopened ring came back at once"
        );
        assert_eq!(ring.held(), 1, "the reopen let go of a held slot");
        assert!(ring.release(held.index));
        // What the next producer publishes comes out as ever.
        let index = ring.begin().unwrap();
        ring.set(index, 8);
        ring.publish(index);
        assert_eq!(ring.acquire(0, Duration::ZERO).unwrap().payload, 8);
    }

    #[test]
    fn a_publish_wakes_a_waiter() {
        let ring = std::sync::Arc::new(Latest::<u32, 4>::new(0));
        let waiter = {
            let ring = ring.clone();
            std::thread::spawn(move || ring.acquire(0, Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(20));
        let index = ring.begin().unwrap();
        ring.set(index, 42);
        ring.publish(index);
        assert_eq!(waiter.join().unwrap().unwrap().payload, 42);
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    /// The consumer holds a slot while the producer publishes past it and
    /// steals; the held slot must never be filled under the consumer, and
    /// what the consumer reads must be whole.
    #[test]
    fn a_held_slot_is_never_filled_under_its_consumer() {
        loom::model(|| {
            let ring = loom::sync::Arc::new(Latest::<[u32; 2], 2>::new([0, 0]));

            let index = ring.begin().expect("a slot");
            ring.set(index, [1, 1]);
            ring.publish(index);

            let consumer = {
                let ring = ring.clone();
                loom::thread::spawn(move || {
                    // No sleeping inside the model: the futex is real and
                    // the scheduler is not. Poll and yield instead.
                    let taken = loop {
                        if let Some(taken) = ring.acquire(0, Duration::ZERO) {
                            break taken;
                        }
                        loom::thread::yield_now();
                    };
                    let [a, b] = taken.payload;
                    assert_eq!(a, b, "a torn payload");
                    // Held across the producer's next publishes; the payload
                    // read again is unchanged.
                    let again = ring.slots[taken.index].payload.with(|p| unsafe { *p });
                    assert_eq!(again, taken.payload, "the held slot was rewritten");
                    ring.release(taken.index);
                })
            };

            let producer = {
                let ring = ring.clone();
                loom::thread::spawn(move || {
                    for n in 2..4u32 {
                        let index = ring.begin().expect("a slot");
                        ring.set(index, [n, n]);
                        ring.publish(index);
                    }
                })
            };

            consumer.join().expect("consumer");
            producer.join().expect("producer");
        });
    }

    /// The consumer and a reopen race for a slot the close left published:
    /// one of them has it, and a slot the consumer took is never let go
    /// under it.
    #[test]
    fn a_reopen_never_lets_go_of_a_slot_the_consumer_took() {
        loom::model(|| {
            let ring = loom::sync::Arc::new(Latest::<[u32; 2], 2>::new([0, 0]));
            let index = ring.begin().expect("a slot");
            ring.set(index, [1, 1]);
            ring.publish(index);
            ring.close();

            let consumer = {
                let ring = ring.clone();
                loom::thread::spawn(move || ring.acquire(0, Duration::ZERO))
            };
            ring.reopen();
            match consumer.join().expect("consumer") {
                Some(taken) => {
                    assert_eq!(taken.payload, [1, 1], "a torn payload");
                    assert_eq!(ring.held(), 1, "a slot the consumer took was let go");
                }
                None => assert_eq!(
                    ring.ready() + ring.held(),
                    0,
                    "the slot the close left outlived the reopen"
                ),
            }
        });
    }
}
