//! A pool of byte slots, filled by one thread and read by others by index.
//!
//! **One copy, many readers.** A producer's own buffer is valid only until
//! its next use, so a finished unit is copied once into a slot here and every
//! consumer is handed the slot's index rather than a copy of its own. A host
//! publishes one encoded picture to every guest this way (docs/05-host.md
//! section 6); a client hands each received access unit from its receive loop
//! to its decoder the same way.
//!
//! One thread writes bytes another thread reads, so the rule from
//! docs/impl-plan.md phase 0 applies: it is model checked, and the model check
//! is shown capable of failing rather than trusted.
//!
//! **The refcount is the only thing that says a slot is reusable.** A slot is
//! taken while it is being written, held once per ring it was published to,
//! and released as each consumer finishes with it. Reaching zero is what
//! returns it to the producer, and nothing else does.

use crate::spsc::Ring;
use crate::sync::{AtomicU32, AtomicUsize, Ordering, UnsafeCell};

/// One unit's storage and the count of who still needs it.
struct Slot {
    /// **Zero means the producer may reuse this slot**, and it is read by the
    /// producer while consumers are decrementing it, so it sits alone in its
    /// own cache line.
    holders: AtomicUsize,
    /// How much of `bytes` the unit occupies. Ordered by the ring the index
    /// travels on, not by itself, so plain ordering is enough.
    len: AtomicUsize,
    /// A word the producer attaches and the consumer reads back, carried
    /// beside the bytes because it describes them: a host marks a keyframe, a
    /// client marks a message that is metadata rather than a picture.
    tag: AtomicU32,
    /// Written only while the slot is held by its writer and read only while
    /// it is held by a consumer, which is what makes the sharing sound.
    bytes: UnsafeCell<Box<[u8]>>,
}

// SAFETY: every access to `bytes` is gated by `holders`. The producer writes
// only between taking a slot at zero and publishing it, and a consumer reads
// only while it holds a count it has not yet released. The two windows cannot
// overlap, because the producer cannot take a slot whose count is nonzero.
unsafe impl Sync for Slot {}
// SAFETY: as above; the storage owns nothing thread-affine.
unsafe impl Send for Slot {}

/// A fixed pool of byte slots.
///
/// Allocated once at session setup and never grown. Publishing costs an index
/// and a counter, never an allocation and never a copy per consumer.
#[derive(Debug)]
pub struct Pool {
    slots: Box<[Slot]>,
    /// Where the last search stopped, so a scan does not always begin at zero
    /// and wear the same slots. A hint only: being stale costs one step of a
    /// scan and can cost nothing else.
    hint: AtomicUsize,
}

impl core::fmt::Debug for Slot {
    /// The bytes are a unit and say nothing useful in a log.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Slot")
            .field("holders", &self.holders.load(Ordering::Relaxed))
            .field("len", &self.len.load(Ordering::Relaxed))
            .finish()
    }
}

impl Pool {
    /// Allocate `slots` slots of `bytes` each.
    ///
    /// **Sized from the largest unit the stream can produce**, not from an
    /// average: a refresh of a hard scene is many times the mean, and a slot
    /// too small refuses the unit rather than truncating it.
    pub fn new(slots: usize, bytes: usize) -> Self {
        let slots = (0..slots)
            .map(|_| Slot {
                holders: AtomicUsize::new(0),
                len: AtomicUsize::new(0),
                tag: AtomicU32::new(0),
                bytes: UnsafeCell::new(vec![0u8; bytes].into_boxed_slice()),
            })
            .collect();
        Self {
            slots,
            hint: AtomicUsize::new(0),
        }
    }

    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Take a free slot to write into, or `None` while every one is still held.
    ///
    /// **`None` is back pressure, not a fault.** It means the consumers are
    /// behind; what to do about that is the caller's decision and not this
    /// type's.
    ///
    /// Called only by the producing thread. Two callers would be able to take
    /// the same slot, which is the same single-producer contract the rings
    /// carry.
    pub fn acquire(&self) -> Option<Writer<'_>> {
        let count = self.slots.len();
        let start = self.hint.load(Ordering::Relaxed);
        for step in 0..count {
            let index = (start + step) % count;
            let slot = self.slots.get(index)?;
            // Acquire, so the writes of whichever consumer released it last are
            // visible before this slot is written again.
            if slot.holders.load(Ordering::Acquire) == 0 {
                // Held by the writer itself from here, so a second acquire
                // cannot hand out the same slot before it is published.
                slot.holders.store(1, Ordering::Relaxed);
                self.hint.store((index + 1) % count, Ordering::Relaxed);
                return Some(Writer {
                    pool: self,
                    index,
                    len: 0,
                });
            }
        }
        None
    }

    /// Take a hold on a slot an index names.
    ///
    /// The count was already raised on this consumer's behalf when the unit
    /// was published, so this transfers that hold into something that
    /// releases itself.
    pub fn claim(&self, index: u32) -> Option<Frame<'_>> {
        let index = usize::try_from(index).ok()?;
        self.slots.get(index)?;
        Some(Frame { pool: self, index })
    }

    fn release(&self, index: usize) {
        let Some(slot) = self.slots.get(index) else {
            return;
        };
        // Release, so everything this holder did is visible to the producer
        // that next finds the slot free.
        slot.holders.fetch_sub(1, Ordering::Release);
    }

    /// Slots the producer could take right now. Observability for the tests
    /// that assert every hold comes back, which is the invariant the whole
    /// type rests on.
    pub fn free_slots(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.holders.load(Ordering::Acquire) == 0)
            .count()
    }

    /// How many consumers still hold a slot. Observability for the tests that
    /// assert the count returns to zero, which is the invariant the whole
    /// type rests on.
    #[cfg(test)]
    fn holders(&self, index: usize) -> usize {
        self.slots
            .get(index)
            .map_or(0, |slot| slot.holders.load(Ordering::Acquire))
    }
}

/// A slot taken for writing, before anyone else can see it.
#[derive(Debug)]
pub struct Writer<'a> {
    pool: &'a Pool,
    index: usize,
    len: usize,
}

impl Writer<'_> {
    /// Copy a finished unit in.
    ///
    /// Returns `false` if it does not fit, which is a sizing error rather than
    /// a transient one: the slot size is chosen from the largest unit the
    /// stream can produce, so this means that estimate was wrong.
    pub fn fill(&mut self, bitstream: &[u8]) -> bool {
        self.fill_with(|storage| {
            if bitstream.len() > storage.len() {
                return None;
            }
            storage
                .get_mut(..bitstream.len())?
                .copy_from_slice(bitstream);
            Some(bitstream.len())
        })
    }

    /// Let `write` produce the unit straight into the slot, and report how
    /// many bytes it wrote; `None` leaves the slot unfilled.
    ///
    /// For a producer whose source hands bytes out by writing into a buffer
    /// it is given, so the copy `fill` makes is not made twice.
    pub fn fill_with(&mut self, write: impl FnOnce(&mut [u8]) -> Option<usize>) -> bool {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return false;
        };
        // SAFETY: the slot is held by this writer alone. It was taken at a
        // count of zero and raised to one before this value existed, so no
        // consumer holds it and the producer cannot take it again.
        slot.bytes.with_mut(|bytes| {
            // SAFETY: as above, and the pointer is to a live boxed slice.
            let storage = unsafe { &mut *bytes };
            match write(storage) {
                Some(len) if len <= storage.len() => {
                    self.len = len;
                    true
                }
                _ => false,
            }
        })
    }

    /// What has been written so far. The producer's own view of its slot,
    /// which is what lets it classify a unit it took straight off the wire
    /// before publishing it.
    pub fn written(&self) -> &[u8] {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return &[];
        };
        // SAFETY: the slot is held by this writer alone, as in `fill_with`,
        // and nothing else reads or writes it until it is published.
        slot.bytes.with(|bytes| {
            // SAFETY: as above; the pointer is to a live boxed slice.
            let storage = unsafe { &*bytes };
            storage.get(..self.len).unwrap_or(&[])
        })
    }

    /// Publish to every ring that will take it, and report **which ones did**,
    /// as a bit per ring in the order they were given.
    ///
    /// **A count would not be enough.** A ring that refuses is a consumer
    /// that missed a unit, and on a host a guest that misses one frame must
    /// miss every frame until a keyframe; the caller can only latch the right
    /// consumer if it is told which one. Returning how many took it leaves the
    /// caller knowing a unit was lost and unable to act on it, which is the
    /// silent form of the failure a delivery gate exists to prevent.
    ///
    /// **The count is raised before any index is pushed.** Raising it after
    /// would let the first consumer finish and release the slot to zero while
    /// later ones were still being handed the same index, and the producer
    /// would then be free to overwrite a unit that had not been read yet. A
    /// ring that refuses gives its hold straight back.
    ///
    /// At most 32 rings.
    pub fn publish<const D: usize>(self, tag: u32, rings: &[&Ring<u32, D>]) -> u32 {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return 0;
        };
        slot.len.store(self.len, Ordering::Relaxed);
        slot.tag.store(tag, Ordering::Relaxed);
        slot.holders.fetch_add(rings.len(), Ordering::Relaxed);

        let index = u32::try_from(self.index).unwrap_or(u32::MAX);
        let mut taken = 0u32;
        for (at, ring) in rings.iter().enumerate() {
            if ring.push(index).is_ok() {
                taken |= 1u32 << (at % 32);
            } else {
                // It never arrived, so the hold raised for it is given back.
                // A full ring is the caller's business, not the pool's.
                self.pool.release(self.index);
            }
        }
        // The writer's own hold is dropped by `Drop` as this returns, which is
        // after every push above. Releasing it here as well would take the
        // count down twice and hand the slot back while a reader still held it.
        taken
    }
}

impl Drop for Writer<'_> {
    /// Gives back the hold taken when the slot was acquired.
    ///
    /// **One place, both paths.** A published unit reaches here after its
    /// readers have been counted, and an abandoned one reaches here with
    /// nothing else holding it, so the slot returns either way and neither
    /// path can release it twice.
    fn drop(&mut self) {
        self.pool.release(self.index);
    }
}

/// One consumer's hold on a published unit.
///
/// **Releases itself.** The consumer is the only thing that decides when a
/// unit is finished with, and tying the release to this value means it cannot
/// be forgotten on a path that returns early.
#[derive(Debug)]
pub struct Frame<'a> {
    pool: &'a Pool,
    index: usize,
}

impl Frame<'_> {
    /// The bytes. Valid for as long as this hold is.
    pub fn bytes(&self) -> &[u8] {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return &[];
        };
        let len = slot.len.load(Ordering::Relaxed);
        // SAFETY: this hold is one of the counts raised at publish and has not
        // been released, so the producer cannot have taken the slot back and
        // cannot be writing. Other consumers may read the same bytes at the
        // same time, which is a shared read.
        slot.bytes.with(|bytes| {
            // SAFETY: as above; the pointer is to a live boxed slice.
            let storage = unsafe { &*bytes };
            storage.get(..len).unwrap_or(&[])
        })
    }

    /// The word the producer attached at publish.
    pub fn tag(&self) -> u32 {
        self.pool
            .slots
            .get(self.index)
            .map_or(0, |slot| slot.tag.load(Ordering::Relaxed))
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        self.pool.release(self.index);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    const DEPTH: usize = 4;

    #[test]
    fn a_published_unit_reaches_every_consumer_and_the_slot_returns_once_they_are_done() {
        let pool = Pool::new(2, 64);
        let one = Ring::<u32, DEPTH>::new();
        let two = Ring::<u32, DEPTH>::new();

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"a frame"));
        assert_eq!(writer.publish(1, &[&one, &two]), 0b11);

        // Still held by both consumers, so the producer cannot have it back.
        assert_eq!(pool.holders(0), 2);

        let first = pool.claim(one.pop().expect("published")).expect("slot");
        assert_eq!(first.bytes(), b"a frame");
        assert_eq!(first.tag(), 1);
        drop(first);
        assert_eq!(pool.holders(0), 1, "one consumer finishing freed the slot");

        let second = pool.claim(two.pop().expect("published")).expect("slot");
        assert_eq!(
            second.bytes(),
            b"a frame",
            "the second consumer sees the same"
        );
        drop(second);
        assert_eq!(pool.holders(0), 0, "the slot did not come back");
    }

    /// **The whole point of a pool.** One write, one copy, many readers.
    #[test]
    fn the_unit_is_stored_once_however_many_consumers_take_it() {
        let pool = Pool::new(1, 64);
        let rings: [Ring<u32, DEPTH>; 3] = [Ring::new(), Ring::new(), Ring::new()];
        let borrowed: Vec<&Ring<u32, DEPTH>> = rings.iter().collect();

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"one copy"));
        assert_eq!(writer.publish(0, &borrowed), 0b111);

        let held: Vec<_> = rings
            .iter()
            .map(|ring| pool.claim(ring.pop().expect("published")).expect("slot"))
            .collect();
        assert!(held.iter().all(|frame| frame.bytes() == b"one copy"));
        // Every consumer is looking at the same storage, not at a copy.
        let first = held[0].bytes().as_ptr();
        assert!(held.iter().all(|frame| frame.bytes().as_ptr() == first));
    }

    #[test]
    fn a_pool_whose_slots_are_all_held_refuses_rather_than_growing() {
        let pool = Pool::new(1, 64);
        let ring = Ring::<u32, DEPTH>::new();

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"held"));
        writer.publish(0, &[&ring]);

        assert!(
            pool.acquire().is_none(),
            "handed out a slot still in flight"
        );
        drop(pool.claim(ring.pop().expect("published")));
        assert!(pool.acquire().is_some(), "the slot never came back");
    }

    /// A ring that will not take the frame must give its hold straight back,
    /// or the slot is never reusable again and the pool bleeds one slot per
    /// congested consumer until it stops entirely.
    #[test]
    fn a_refused_push_does_not_strand_the_slot() {
        let pool = Pool::new(1, 64);
        let full = Ring::<u32, 1>::new();
        full.push(99).expect("room");

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"nowhere to go"));
        assert_eq!(writer.publish(0, &[&full]), 0, "a full ring took it");
        assert_eq!(pool.holders(0), 0, "the slot was stranded");
        assert!(pool.acquire().is_some());
    }

    #[test]
    fn a_slot_written_and_never_published_goes_back() {
        let pool = Pool::new(1, 64);
        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"abandoned"));
        drop(writer);
        assert_eq!(pool.holders(0), 0);
        assert!(pool.acquire().is_some());
    }

    #[test]
    fn a_unit_larger_than_a_slot_is_refused_rather_than_truncated() {
        let pool = Pool::new(1, 8);
        let mut writer = pool.acquire().expect("a free slot");
        assert!(!writer.fill(b"far longer than eight bytes"));
    }

    /// A writer that produces in place is given the whole slot and believed
    /// about the length, but never past the slot's end.
    #[test]
    fn a_unit_written_in_place_is_published_at_the_length_the_writer_said() {
        let pool = Pool::new(1, 8);
        let ring = Ring::<u32, DEPTH>::new();
        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill_with(|slot| {
            assert_eq!(slot.len(), 8);
            slot[..3].copy_from_slice(b"abc");
            Some(3)
        }));
        writer.publish(7, &[&ring]);
        let held = pool.claim(ring.pop().expect("published")).expect("slot");
        assert_eq!(held.bytes(), b"abc");
        assert_eq!(held.tag(), 7);
        drop(held);

        let mut writer = pool.acquire().expect("a free slot");
        assert!(
            !writer.fill_with(|_| Some(9)),
            "a length past the slot was believed"
        );
        assert!(
            !writer.fill_with(|_| None),
            "an unfilled slot reads as filled"
        );
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    /// The handoff, explored rather than reasoned about: the producer writes
    /// and publishes, a consumer reads and releases, and the producer takes
    /// the slot again and writes it a second time.
    ///
    /// **What this is looking for** is the producer reusing a slot while a
    /// consumer is still reading it. That needs the release on the consumer's
    /// decrement to pair with the acquire on the producer's search; weaken
    /// either and loom finds the interleaving where the second write lands
    /// under the first reader.
    #[test]
    fn a_slot_is_never_rewritten_while_a_consumer_still_holds_it() {
        loom::model(|| {
            let pool = loom::sync::Arc::new(Pool::new(1, 8));
            let ring = loom::sync::Arc::new(Ring::<u32, 2>::new());

            let mut writer = pool.acquire().expect("a free slot");
            assert!(writer.fill(&[1, 1, 1, 1]));
            assert_eq!(writer.publish(0, &[&ring]), 0b1);

            let consumer = {
                let pool = pool.clone();
                let ring = ring.clone();
                loom::thread::spawn(move || {
                    let index = loop {
                        if let Some(index) = ring.pop() {
                            break index;
                        }
                        loom::thread::yield_now();
                    };
                    let frame = pool.claim(index).expect("slot");
                    let bytes = frame.bytes();
                    // Every byte of a frame comes from one writer, so a torn
                    // read is a mixture and shows up here.
                    assert!(
                        bytes.iter().all(|byte| *byte == bytes[0]),
                        "a slot was rewritten under a reader"
                    );
                })
            };

            // The producer wants the slot back and must not get it early.
            let producer = {
                let pool = pool.clone();
                loom::thread::spawn(move || {
                    loop {
                        if let Some(mut writer) = pool.acquire() {
                            assert!(writer.fill(&[2, 2, 2, 2]));
                            break;
                        }
                        loom::thread::yield_now();
                    }
                })
            };

            consumer.join().expect("consumer");
            producer.join().expect("producer");
        });
    }
}
