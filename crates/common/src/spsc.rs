//! Bounded single-producer single-consumer ring.
//!
//! Fixed capacity, no allocation after construction, never blocks, never grows.
//! A full ring refuses the push and hands the value back; the caller decides
//! whether that means drop-oldest or backpressure, because those differ per
//! channel.
//!
//! Correctness is model checked under loom rather than trusted to review; see
//! docs/08-testing.md 7.

use core::mem::MaybeUninit;

use crate::sync::{AtomicUsize, Ordering, UnsafeCell};

/// Keeps the producer's and consumer's counters off each other's cache line.
#[repr(align(64))]
struct Padded<T>(T);

/// A ring of capacity `N`.
///
/// # Safety contract
///
/// Exactly one thread may call [`Ring::push`] and exactly one may call
/// [`Ring::pop`], for the lifetime of the ring. Two producers or two consumers
/// is undefined behavior. The type is `Sync` so the two ends can live on
/// different threads; it is not a multi-producer queue.
pub struct Ring<T, const N: usize> {
    slots: [UnsafeCell<MaybeUninit<T>>; N],
    /// Consumer-owned. Counts pops, monotonically, wrapping.
    head: Padded<AtomicUsize>,
    /// Producer-owned. Counts pushes, monotonically, wrapping.
    tail: Padded<AtomicUsize>,
}

// SAFETY: access is disjoint by the single-producer single-consumer contract
// above, and published across threads by the release/acquire pairing on head
// and tail.
unsafe impl<T: Send, const N: usize> Send for Ring<T, N> {}
// SAFETY: as above.
unsafe impl<T: Send, const N: usize> Sync for Ring<T, N> {}

impl<T, const N: usize> Ring<T, N> {
    /// Create an empty ring.
    ///
    /// # Panics
    ///
    /// If `N` is zero. This is a programming error, not an input error, and it
    /// happens at construction rather than on a data path.
    pub fn new() -> Self {
        assert!(N > 0, "ring capacity must be non-zero");
        Self {
            slots: core::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
            head: Padded(AtomicUsize::new(0)),
            tail: Padded(AtomicUsize::new(0)),
        }
    }

    /// Capacity in items.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Items currently queued.
    pub fn len(&self) -> usize {
        let tail = self.tail.0.load(Ordering::Acquire);
        let head = self.head.0.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True if a push would fail.
    pub fn is_full(&self) -> bool {
        self.len() >= N
    }

    /// Push a value. Producer thread only.
    ///
    /// Returns the value back in `Err` if the ring is full. Never blocks.
    pub fn push(&self, value: T) -> Result<(), T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail.wrapping_sub(head) >= N {
            return Err(value);
        }
        let index = tail % N;
        self.slots[index].with_mut(|slot| {
            // SAFETY: the slot is free. `head` has advanced past it, so the
            // consumer has already taken whatever was there, and we are the
            // only producer.
            unsafe {
                (*slot).write(value);
            }
        });
        // Release: the write above must be visible before the consumer can
        // observe the new tail.
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Pop a value. Consumer thread only.
    pub fn pop(&self) -> Option<T> {
        let head = self.head.0.load(Ordering::Relaxed);
        // Acquire: pairs with the producer's release store, making its slot
        // write visible to us.
        let tail = self.tail.0.load(Ordering::Acquire);
        if head == tail {
            return None;
        }
        let index = head % N;
        let value = self.slots[index].with(|slot| {
            // SAFETY: tail has passed this slot, so the producer initialised it
            // and will not touch it again until we release it below.
            unsafe { (*slot).assume_init_read() }
        });
        // Release: the read above must complete before the producer can reuse
        // the slot.
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }
}

impl<T, const N: usize> Default for Ring<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Drop for Ring<T, N> {
    fn drop(&mut self) {
        while self.pop().is_some() {}
    }
}

impl<T, const N: usize> core::fmt::Debug for Ring<T, N> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ring")
            .field("capacity", &N)
            .field("len", &self.len())
            .finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn fifo_order() {
        let ring: Ring<u32, 4> = Ring::new();
        assert!(ring.is_empty());
        for i in 0..4 {
            ring.push(i).unwrap();
        }
        assert!(ring.is_full());
        for i in 0..4 {
            assert_eq!(ring.pop(), Some(i));
        }
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn full_ring_hands_the_value_back() {
        let ring: Ring<u32, 2> = Ring::new();
        ring.push(1).unwrap();
        ring.push(2).unwrap();
        assert_eq!(ring.push(3), Err(3));
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn indices_wrap_without_losing_order() {
        let ring: Ring<usize, 3> = Ring::new();
        for round in 0..100 {
            ring.push(round).unwrap();
            assert_eq!(ring.pop(), Some(round));
        }
        assert!(ring.is_empty());
    }

    #[test]
    fn drop_releases_queued_items() {
        use std::rc::Rc;
        let witness = Rc::new(());
        {
            let ring: Ring<Rc<()>, 4> = Ring::new();
            ring.push(Rc::clone(&witness)).unwrap();
            ring.push(Rc::clone(&witness)).unwrap();
            assert_eq!(Rc::strong_count(&witness), 3);
        }
        assert_eq!(Rc::strong_count(&witness), 1, "dropped ring leaked items");
    }

    #[test]
    fn crosses_threads() {
        use std::sync::Arc;
        const COUNT: usize = 10_000;
        let ring: Arc<Ring<usize, 64>> = Arc::new(Ring::new());
        let producer_ring = Arc::clone(&ring);
        let producer = std::thread::spawn(move || {
            for i in 0..COUNT {
                while producer_ring.push(i).is_err() {
                    std::thread::yield_now();
                }
            }
        });
        let mut received = 0;
        while received < COUNT {
            match ring.pop() {
                Some(value) => {
                    assert_eq!(value, received);
                    received += 1;
                }
                None => std::thread::yield_now(),
            }
        }
        producer.join().unwrap();
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    /// Phase 0 gate: the ring is model checked, not soak tested.
    #[test]
    fn spsc_interleavings() {
        loom::model(|| {
            let ring = loom::sync::Arc::new(Ring::<u32, 2>::new());

            let producer_ring = ring.clone();
            let producer = loom::thread::spawn(move || {
                // Capacity is 2 and we push 2, so this never blocks and the
                // state space stays tractable.
                producer_ring.push(1).unwrap();
                producer_ring.push(2).unwrap();
            });

            let consumer_ring = ring.clone();
            let consumer = loom::thread::spawn(move || {
                let mut seen = Vec::new();
                while seen.len() < 2 {
                    if let Some(value) = consumer_ring.pop() {
                        seen.push(value);
                    } else {
                        loom::thread::yield_now();
                    }
                }
                assert_eq!(seen, vec![1, 2], "order violated under interleaving");
            });

            producer.join().unwrap();
            consumer.join().unwrap();
        });
    }
}
