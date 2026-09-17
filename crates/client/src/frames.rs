//! The decoded-picture queue: four slots of planes between the decode thread
//! and the application (docs/10-client.md section 4).
//!
//! The ordering is the common ring's; what is here is the storage. The
//! slots are sized once, from the configuration's ceiling at the deepest
//! layout a decoder here produces, and **backed on the decode thread at the
//! first decoder build**, demand-zero: nothing is allocated at creation or
//! at an attempt, and the working set is the pictures actually written,
//! never the reserve. A rebuild never reallocates, so a slot the application
//! holds is never pulled from under it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lowlat_common::latest::{Latest, Taken};
use lowlat_core::video::Rotation;
use lowlat_decode::{Format, Planes};

/// Slots: two the application may hold, one being decoded into, one ready.
pub const SLOTS: usize = 4;
/// The most the application may hold at once.
pub const MAX_HELD: usize = 2;

/// What a slot holds, as the application is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
    pub chroma_444: bool,
    /// The encoder generation the picture belongs to.
    pub generation: u32,
    /// The picture's order in its stream, from the bitstream.
    pub order: i32,
}

impl Frame {
    const BLANK: Self = Self {
        format: Format::Nv12,
        width: 0,
        height: 0,
        rotation: Rotation::None,
        chroma_444: false,
        generation: 0,
        order: 0,
    };
}

/// The one allocation, made on the decode thread.
struct Backing {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the bytes are written only inside a slot the ring has lent the
// producer and read only inside one it has lent the consumer, and the ring's
// state transitions carry the ordering. The pointer itself is set once,
// before the first publish, through the lock.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}

impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: made by `Box::into_raw` on a boxed slice of `len` bytes,
        // freed once.
        unsafe {
            drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(
                self.ptr, self.len,
            )));
        }
    }
}

/// The queue.
pub struct Frames {
    ring: Latest<Frame, SLOTS>,
    /// Bytes per row, for the widest sample at the ceiling, aligned.
    pitch: usize,
    /// Luma rows at the ceiling.
    rows: u32,
    backing: OnceLock<Backing>,
    /// Slots the consumer holds: the two-held rule is enforced here, before
    /// the ring is asked.
    held: AtomicUsize,
}

impl core::fmt::Debug for Frames {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frames")
            .field("pitch", &self.pitch)
            .field("rows", &self.rows)
            .field("backed", &self.backing.get().is_some())
            .finish()
    }
}

/// A slot lent to the producer.
#[derive(Debug)]
pub struct Filling<'a> {
    frames: &'a Frames,
    index: usize,
    /// Set once the slot is published, so the drop does not give it back.
    published: bool,
}

/// The application already holds as many pictures as it may.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooManyHeld;

/// A slot lent to the consumer.
#[derive(Debug, Clone, Copy)]
pub struct Held {
    pub index: usize,
    pub seq: u64,
    pub frame: Frame,
    pub y: *const u8,
    pub uv: *const u8,
    pub pitch: usize,
}

impl Frames {
    /// A queue for pictures up to `ceiling`.
    pub fn new(ceiling: (u32, u32)) -> Self {
        let width = usize::try_from(ceiling.0.max(16)).unwrap_or(16);
        // Two bytes a sample at the deepest layout, rows aligned to a cache
        // line.
        let pitch = (width * 2).div_ceil(64) * 64;
        Self {
            ring: Latest::new(Frame::BLANK),
            pitch,
            rows: ceiling.1.max(16),
            backing: OnceLock::new(),
            held: AtomicUsize::new(0),
        }
    }

    fn slot_bytes(&self) -> usize {
        let rows = usize::try_from(self.rows).unwrap_or(16);
        self.pitch * rows + self.pitch * rows.div_ceil(2)
    }

    /// Whether the slots are backed yet.
    pub fn backed(&self) -> bool {
        self.backing.get().is_some()
    }

    /// Bytes the slots reserve, backed or not.
    pub fn reserve_bytes(&self) -> usize {
        self.slot_bytes() * SLOTS
    }

    fn backing(&self) -> &Backing {
        self.backing.get_or_init(|| {
            // Demand-zero: the pages are mapped when a picture is first
            // written into them, never here.
            let len = self.slot_bytes() * SLOTS;
            let boxed = vec![0u8; len].into_boxed_slice();
            let ptr = Box::into_raw(boxed).cast::<u8>();
            Backing { ptr, len }
        })
    }

    fn slot_ptr(&self, index: usize) -> *mut u8 {
        let backing = self.backing();
        let offset = (index % SLOTS) * self.slot_bytes();
        // SAFETY: `offset` is inside the allocation for every slot index.
        unsafe { backing.ptr.add(offset) }
    }

    /// The producer takes a slot to decode into. `None` only if every slot
    /// is held or filling, which the two-held rule prevents.
    pub fn fill(&self) -> Option<Filling<'_>> {
        let index = self.ring.begin()?;
        Some(Filling {
            frames: self,
            index,
            published: false,
        })
    }

    /// The consumer takes the newest picture after `after`, waiting up to
    /// `timeout`. `Err` when it already holds the most it may.
    pub fn acquire(&self, after: u64, timeout: Duration) -> Result<Option<Held>, TooManyHeld> {
        if self.held.load(Ordering::Acquire) >= MAX_HELD {
            return Err(TooManyHeld);
        }
        let Some(Taken {
            index,
            seq,
            payload,
        }) = self.ring.acquire(after, timeout)
        else {
            return Ok(None);
        };
        self.held.fetch_add(1, Ordering::AcqRel);
        let y = self.slot_ptr(index).cast_const();
        let rows = usize::try_from(self.rows).unwrap_or(16);
        // SAFETY: inside the slot.
        let uv = unsafe { y.add(self.pitch * rows) };
        Ok(Some(Held {
            index,
            seq,
            frame: payload,
            y,
            uv,
            pitch: self.pitch,
        }))
    }

    /// The consumer is done with a slot it holds.
    pub fn release(&self, index: usize) {
        if self.ring.release(index) {
            let _ = self
                .held
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |h| h.checked_sub(1));
        }
    }

    /// No more pictures are coming: wake every waiter.
    pub fn close(&self) {
        self.ring.close();
    }

    /// Pictures published and not yet taken.
    pub fn ready(&self) -> usize {
        self.ring.ready()
    }

    /// Slots the application holds.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Relaxed)
    }

    /// The largest picture a slot takes.
    pub fn ceiling(&self) -> (u32, u32) {
        (u32::try_from(self.pitch / 2).unwrap_or(0), self.rows)
    }
}

impl Filling<'_> {
    /// The planes to decode into, at the queue's pitch.
    pub fn planes(&mut self) -> Planes<'_> {
        let rows = usize::try_from(self.frames.rows).unwrap_or(16);
        let pitch = self.frames.pitch;
        let base = self.frames.slot_ptr(self.index);
        // SAFETY: the slot is lent to this producer alone until it is
        // published or abandoned; the two ranges are disjoint and inside the
        // slot.
        let (y, uv) = unsafe {
            (
                core::slice::from_raw_parts_mut(base, pitch * rows),
                core::slice::from_raw_parts_mut(base.add(pitch * rows), pitch * rows.div_ceil(2)),
            )
        };
        Planes {
            y,
            y_pitch: pitch,
            uv,
            uv_pitch: pitch,
        }
    }

    /// The picture is in the slot: publish it as the newest.
    pub fn publish(mut self, frame: Frame) {
        self.frames.ring.set(self.index, frame);
        self.frames.ring.publish(self.index);
        self.published = true;
    }
}

impl Drop for Filling<'_> {
    fn drop(&mut self) {
        // Dropped without publishing: the slot goes back unused.
        if !self.published {
            self.frames.ring.abandon(self.index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(order: i32) -> Frame {
        Frame {
            order,
            width: 64,
            height: 64,
            ..Frame::BLANK
        }
    }

    #[test]
    fn nothing_is_backed_until_the_first_picture() {
        let frames = Frames::new((4096, 4096));
        assert!(!frames.backed());
        assert_eq!(frames.reserve_bytes(), 4 * (8192 * 4096 + 8192 * 2048));
        let mut filling = frames.fill().unwrap();
        let planes = filling.planes();
        assert_eq!(planes.y_pitch, 8192);
        assert!(frames.backed());
    }

    #[test]
    fn a_third_hold_is_refused_and_a_release_makes_room() {
        let frames = Frames::new((64, 64));
        let mut last = 0;
        let mut held = Vec::new();
        for n in 0..2 {
            let filling = frames.fill().unwrap();
            filling.publish(frame(n));
            let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
            last = h.seq;
            held.push(h.index);
        }
        let filling = frames.fill().unwrap();
        filling.publish(frame(2));
        assert!(
            frames.acquire(last, Duration::ZERO).is_err(),
            "a third hold"
        );
        frames.release(held[0]);
        let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
        assert_eq!(h.frame.order, 2);
    }

    #[test]
    fn the_producer_never_blocks_with_two_held() {
        let frames = Frames::new((64, 64));
        let mut last = 0;
        for n in 0..2 {
            frames.fill().unwrap().publish(frame(n));
            last = frames.acquire(last, Duration::ZERO).unwrap().unwrap().seq;
        }
        for n in 2..200 {
            let filling = frames.fill().expect("a slot with two held");
            filling.publish(frame(n));
        }
        assert_eq!(frames.held(), 2);
        assert_eq!(frames.ready(), 2);
    }

    #[test]
    fn a_filling_dropped_unpublished_returns_its_slot() {
        let frames = Frames::new((64, 64));
        {
            let mut filling = frames.fill().unwrap();
            let _ = filling.planes();
        }
        assert_eq!(frames.ready(), 0);
        assert!(frames.acquire(0, Duration::ZERO).unwrap().is_none());
    }

    #[test]
    fn what_is_written_is_what_is_read() {
        let frames = Frames::new((64, 64));
        let mut filling = frames.fill().unwrap();
        {
            let planes = filling.planes();
            planes.y[0] = 0xAB;
            planes.uv[0] = 0xCD;
        }
        filling.publish(frame(1));
        let h = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        // SAFETY: the slot is held.
        unsafe {
            assert_eq!(*h.y, 0xAB);
            assert_eq!(*h.uv, 0xCD);
        }
        frames.release(h.index);
        assert_eq!(frames.held(), 0);
    }
}
