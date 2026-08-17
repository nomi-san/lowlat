//! The encoded-frame pool, and how one frame reaches many guests.
//!
//! **One encode serves every guest.** The encoder's own buffer is valid only
//! until its next collect, so a finished picture is copied once into a slot
//! here and every guest is handed the slot's index rather than a copy of its
//! own. See docs/05-host.md section 6.
//!
//! That makes the pool the first thing in this workspace where one thread
//! writes bytes another thread reads, so the rule from
//! docs/impl-plan.md phase 0 applies: it is model checked, and the model check
//! is shown capable of failing rather than trusted.
//!
//! **The refcount is the only thing that says a slot is reusable.** A slot is
//! taken while it is being written, held once per guest it was published to,
//! and released as each guest finishes with it. Reaching zero is what returns
//! it to the producer, and nothing else does.

use lowlat_common::spsc::Ring;
use lowlat_common::sync::{AtomicBool, AtomicUsize, Ordering, UnsafeCell};

/// One frame's storage and the count of who still needs it.
struct Slot {
    /// **Zero means the producer may reuse this slot**, and it is read by the
    /// producer while consumers are decrementing it, so it sits alone in its
    /// own cache line.
    holders: AtomicUsize,
    /// How much of `bytes` the frame occupies. Ordered by the ring the index
    /// travels on, not by itself, so plain ordering is enough.
    len: AtomicUsize,
    keyframe: AtomicBool,
    /// Written only while the slot is held by its writer and read only while
    /// it is held by a guest, which is what makes the sharing sound.
    bytes: UnsafeCell<Box<[u8]>>,
}

// SAFETY: every access to `bytes` is gated by `holders`. The producer writes
// only between taking a slot at zero and publishing it, and a consumer reads
// only while it holds a count it has not yet released. The two windows cannot
// overlap, because the producer cannot take a slot whose count is nonzero.
unsafe impl Sync for Slot {}
// SAFETY: as above; the storage owns nothing thread-affine.
unsafe impl Send for Slot {}

/// A fixed pool of encoded frames.
///
/// Allocated once at session setup and never grown. Publishing costs an index
/// and a counter, never an allocation and never a copy per guest.
#[derive(Debug)]
pub struct Pool {
    slots: Box<[Slot]>,
    /// Where the last search stopped, so a scan does not always begin at zero
    /// and wear the same slots. A hint only: being stale costs one step of a
    /// scan and can cost nothing else.
    hint: AtomicUsize,
}

impl core::fmt::Debug for Slot {
    /// The bytes are a frame and say nothing useful in a log.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Slot")
            .field("holders", &self.holders.load(Ordering::Relaxed))
            .field("len", &self.len.load(Ordering::Relaxed))
            .finish()
    }
}

impl Pool {
    /// Allocate `slots` frames of `bytes` each.
    ///
    /// **Sized from the largest frame the stream can produce**, not from an
    /// average: a refresh of a hard scene is many times the mean, and a slot
    /// too small refuses the frame rather than truncating it.
    pub fn new(slots: usize, bytes: usize) -> Self {
        let slots = (0..slots)
            .map(|_| Slot {
                holders: AtomicUsize::new(0),
                len: AtomicUsize::new(0),
                keyframe: AtomicBool::new(false),
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
    /// **`None` is back pressure, not a fault.** It means the guests are
    /// behind; what to do about that is the delivery gate's decision and not
    /// this type's.
    ///
    /// Called only by the thread that owns encoding. Two callers would be able
    /// to take the same slot, which is the same single-producer contract the
    /// rings carry.
    pub fn acquire(&self) -> Option<Writer<'_>> {
        let count = self.slots.len();
        let start = self.hint.load(Ordering::Relaxed);
        for step in 0..count {
            let index = (start + step) % count;
            let slot = self.slots.get(index)?;
            // Acquire, so the writes of whichever guest released it last are
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
    /// The count was already raised on this guest's behalf when the frame was
    /// published, so this transfers that hold into something that releases
    /// itself.
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

    /// How many guests still hold a slot. Observability for the tests that
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
    /// Copy a finished access unit in.
    ///
    /// Returns `false` if the frame does not fit, which is a sizing error
    /// rather than a transient one: the slot size is chosen from the largest
    /// frame the stream can produce, so this means that estimate was wrong.
    pub fn fill(&mut self, bitstream: &[u8]) -> bool {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return false;
        };
        // SAFETY: the slot is held by this writer alone. It was taken at a
        // count of zero and raised to one before this value existed, so no
        // consumer holds it and the producer cannot take it again.
        slot.bytes.with_mut(|bytes| {
            // SAFETY: as above, and the pointer is to a live boxed slice.
            let storage = unsafe { &mut *bytes };
            if bitstream.len() > storage.len() {
                return false;
            }
            let Some(head) = storage.get_mut(..bitstream.len()) else {
                return false;
            };
            head.copy_from_slice(bitstream);
            self.len = bitstream.len();
            true
        })
    }

    /// Publish to every ring that will take it, and report how many did.
    ///
    /// **The count is raised before any index is pushed.** Raising it after
    /// would let the first guest finish and release the slot to zero while
    /// later guests were still being handed the same index, and the producer
    /// would then be free to overwrite a frame that had not been sent yet.
    /// A ring that refuses gives its hold straight back.
    pub fn publish<const D: usize>(self, keyframe: bool, rings: &[&Ring<u32, D>]) -> usize {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return 0;
        };
        slot.len.store(self.len, Ordering::Relaxed);
        slot.keyframe.store(keyframe, Ordering::Relaxed);
        slot.holders.fetch_add(rings.len(), Ordering::Relaxed);

        let index = u32::try_from(self.index).unwrap_or(u32::MAX);
        let mut taken = 0usize;
        for ring in rings {
            if ring.push(index).is_ok() {
                taken += 1;
            } else {
                // It never arrived, so the hold raised for it is given back.
                // A full ring is the gate's business, not the pool's.
                self.pool.release(self.index);
            }
        }
        // The writer's own hold is dropped by `Drop` as this returns, which is
        // after every push above. Releasing it here as well would take the
        // count down twice and hand the slot back while a guest still held it.
        taken
    }
}

impl Drop for Writer<'_> {
    /// Gives back the hold taken when the slot was acquired.
    ///
    /// **One place, both paths.** A published frame reaches here after its
    /// guests have been counted, and an abandoned one reaches here with
    /// nothing else holding it, so the slot returns either way and neither
    /// path can release it twice.
    fn drop(&mut self) {
        self.pool.release(self.index);
    }
}

/// One guest's hold on a published frame.
///
/// **Releases itself.** Packetization is the only thing that decides when a
/// frame is finished with, and tying the release to this value means it cannot
/// be forgotten on a path that returns early.
#[derive(Debug)]
pub struct Frame<'a> {
    pool: &'a Pool,
    index: usize,
}

impl Frame<'_> {
    /// The access unit. Valid for as long as this hold is.
    pub fn bytes(&self) -> &[u8] {
        let Some(slot) = self.pool.slots.get(self.index) else {
            return &[];
        };
        let len = slot.len.load(Ordering::Relaxed);
        // SAFETY: this hold is one of the counts raised at publish and has not
        // been released, so the producer cannot have taken the slot back and
        // cannot be writing. Other guests may read the same bytes at the same
        // time, which is a shared read.
        slot.bytes.with(|bytes| {
            // SAFETY: as above; the pointer is to a live boxed slice.
            let storage = unsafe { &*bytes };
            storage.get(..len).unwrap_or(&[])
        })
    }

    pub fn keyframe(&self) -> bool {
        self.pool
            .slots
            .get(self.index)
            .is_some_and(|slot| slot.keyframe.load(Ordering::Relaxed))
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
    fn a_published_frame_reaches_every_guest_and_the_slot_returns_once_they_are_done() {
        let pool = Pool::new(2, 64);
        let one = Ring::<u32, DEPTH>::new();
        let two = Ring::<u32, DEPTH>::new();

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"a frame"));
        assert_eq!(writer.publish(true, &[&one, &two]), 2);

        // Still held by both guests, so the producer cannot have it back.
        assert_eq!(pool.holders(0), 2);

        let first = pool.claim(one.pop().expect("published")).expect("slot");
        assert_eq!(first.bytes(), b"a frame");
        assert!(first.keyframe());
        drop(first);
        assert_eq!(pool.holders(0), 1, "one guest finishing freed the slot");

        let second = pool.claim(two.pop().expect("published")).expect("slot");
        assert_eq!(second.bytes(), b"a frame", "the second guest sees the same");
        drop(second);
        assert_eq!(pool.holders(0), 0, "the slot did not come back");
    }

    /// **The whole point of a pool.** One encode, one copy, many guests.
    #[test]
    fn the_frame_is_stored_once_however_many_guests_take_it() {
        let pool = Pool::new(1, 64);
        let rings: [Ring<u32, DEPTH>; 3] = [Ring::new(), Ring::new(), Ring::new()];
        let borrowed: Vec<&Ring<u32, DEPTH>> = rings.iter().collect();

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"one copy"));
        assert_eq!(writer.publish(false, &borrowed), 3);

        let held: Vec<_> = rings
            .iter()
            .map(|ring| pool.claim(ring.pop().expect("published")).expect("slot"))
            .collect();
        assert!(held.iter().all(|frame| frame.bytes() == b"one copy"));
        // Every guest is looking at the same storage, not at a copy.
        let first = held[0].bytes().as_ptr();
        assert!(held.iter().all(|frame| frame.bytes().as_ptr() == first));
    }

    #[test]
    fn a_pool_whose_slots_are_all_held_refuses_rather_than_growing() {
        let pool = Pool::new(1, 64);
        let ring = Ring::<u32, DEPTH>::new();

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"held"));
        writer.publish(false, &[&ring]);

        assert!(
            pool.acquire().is_none(),
            "handed out a slot still in flight"
        );
        drop(pool.claim(ring.pop().expect("published")));
        assert!(pool.acquire().is_some(), "the slot never came back");
    }

    /// A ring that will not take the frame must give its hold straight back,
    /// or the slot is never reusable again and the pool bleeds one slot per
    /// congested guest until it stops entirely.
    #[test]
    fn a_refused_push_does_not_strand_the_slot() {
        let pool = Pool::new(1, 64);
        let full = Ring::<u32, 1>::new();
        full.push(99).expect("room");

        let mut writer = pool.acquire().expect("a free slot");
        assert!(writer.fill(b"nowhere to go"));
        assert_eq!(writer.publish(false, &[&full]), 0, "a full ring took it");
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
    fn a_frame_larger_than_a_slot_is_refused_rather_than_truncated() {
        let pool = Pool::new(1, 8);
        let mut writer = pool.acquire().expect("a free slot");
        assert!(!writer.fill(b"far longer than eight bytes"));
    }
}

#[cfg(loom)]
mod loom_tests {
    use super::*;

    /// The handoff, explored rather than reasoned about: the producer writes
    /// and publishes, two guests read and release, and the producer takes the
    /// slot again and writes it a second time.
    ///
    /// **What this is looking for** is the producer reusing a slot while a
    /// guest is still reading it. That needs the release on the consumer's
    /// decrement to pair with the acquire on the producer's search; weaken
    /// either and loom finds the interleaving where the second write lands
    /// under the first reader.
    #[test]
    fn a_slot_is_never_rewritten_while_a_guest_still_holds_it() {
        loom::model(|| {
            let pool = loom::sync::Arc::new(Pool::new(1, 8));
            let ring = loom::sync::Arc::new(Ring::<u32, 2>::new());

            let mut writer = pool.acquire().expect("a free slot");
            assert!(writer.fill(&[1, 1, 1, 1]));
            assert_eq!(writer.publish(false, &[&ring]), 1);

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
