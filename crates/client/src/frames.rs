//! The decoded-picture queue: four slots between the decode thread and the
//! application (docs/10-client.md section 4).
//!
//! The ordering is the common ring's; what is here is the storage, of one
//! of two kinds settled at creation. **Host slots** are sized once, from
//! the configuration's ceiling at the deepest layout a decoder here
//! produces (full chroma at sixteen bits: three planes of two-byte
//! samples), and backed on the decode thread at the first decoder build,
//! demand-zero: nothing is allocated at creation or at an attempt, and the
//! working set is the pictures actually written, never the reserve. Where
//! the system charges memory it has handed out whether it is touched or not,
//! a slot is committed as far as the picture laid out in it reaches, so the
//! charge is the pictures too (the platform's backing, `sys`). A rebuild
//! never reallocates, so a slot the application holds is never pulled from
//! under it.
//!
//! **Device slots** are exportable allocations on the decoder's device,
//! each with a descriptor the application imports, and they are sized at
//! the stream's size rather than the ceiling, because device memory is
//! real where the reserve is virtual. A slot is allocated when a picture of
//! a new layout is about to be decoded into it, and the allocation before
//! it is freed then -- which is after the last hold on it was released,
//! because the ring lends a slot to the producer only once nothing holds
//! it. So the rule is the same as the host slots' by another route.
//!
//! **A picture is laid out at its own pitch, not the ceiling's.** The slot
//! is the reserve; the planes lent to the decoder are `width` samples a
//! row, aligned, with the chroma planes straight after the luma rows, so the
//! pages a picture touches are its own size and nothing more. The layout
//! travels with the published picture, so a held slot keeps its own across
//! anything decoded after it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::Duration;

use lowlat_common::latest::{Latest, Taken};
use lowlat_core::video::Rotation;
use lowlat_decode::{Format, Planes};

use crate::config::FrameKind;

/// How the host slots are backed and what a device slot is, which are the
/// platform's; the queue around them is written once.
#[cfg(target_os = "linux")]
#[path = "frames/linux.rs"]
mod sys;
#[cfg(windows)]
#[path = "frames/windows.rs"]
mod sys;

pub use sys::Handle;
#[cfg(windows)]
pub use sys::Vendor;

/// Slots: two the application may hold, one being decoded into, one ready.
pub const SLOTS: usize = 4;
/// The most the application may hold at once.
pub const MAX_HELD: usize = 2;
/// Rows of a host slot are aligned to a cache line.
const HOST_ALIGN: usize = 64;

/// What a slot holds, as the application is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
    /// The encoder generation the picture belongs to.
    pub generation: u32,
    /// The picture's order in its stream, from the bitstream.
    pub order: i32,
    /// The samples span the whole range, as the stream's parameter set says.
    pub full_range: bool,
    /// The arrival stamp of the unit the picture was decoded from.
    pub arrived: Option<u32>,
    /// Bytes a row, every plane. **The queue's, set at publish** from the
    /// planes it lent; whatever is given here is replaced.
    pub pitch: usize,
    /// Where the chroma planes begin, in bytes from the slot: the
    /// interleaved plane or the first of two, then the second at full chroma
    /// (zero otherwise). The queue's, as `pitch`.
    pub uv_offset: usize,
    pub v_offset: usize,
    /// The device slot's descriptor, or `None` for a host slot. The
    /// queue's, as `pitch`.
    pub handle: Option<Handle>,
}

impl Frame {
    const BLANK: Self = Self {
        format: Format::Nv12,
        width: 0,
        height: 0,
        rotation: Rotation::None,
        generation: 0,
        order: 0,
        full_range: false,
        arrived: None,
        pitch: 0,
        uv_offset: 0,
        v_offset: 0,
        handle: None,
    };
}

/// Where a picture's planes lie in a slot: rows of the picture's own
/// width at `align`, the chroma planes straight after the luma rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    pitch: usize,
    uv_offset: usize,
    v_offset: usize,
    /// Bytes the planes span.
    bytes: usize,
}

impl Layout {
    fn of(width: u32, height: u32, format: Format, align: usize) -> Option<Self> {
        let width = usize::try_from(width).ok()?;
        let height = usize::try_from(height).ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        let pitch = (width * format.sample()).div_ceil(align) * align;
        let luma = pitch.checked_mul(height)?;
        let chroma = pitch.checked_mul(format.chroma_rows(height))?;
        let planes = if format.full_chroma() { 2 } else { 1 };
        let bytes = luma.checked_add(chroma.checked_mul(planes)?)?;
        Some(Self {
            pitch,
            uv_offset: luma,
            v_offset: if format.full_chroma() {
                luma + chroma
            } else {
                0
            },
            bytes,
        })
    }
}

/// The queue.
pub struct Frames {
    ring: Latest<Frame, SLOTS>,
    /// Bytes per row, for the widest sample at the ceiling, aligned.
    pitch: usize,
    /// Luma rows at the ceiling.
    rows: u32,
    /// The kind the session asks for: 0 planes, 1 handles. Read per picture
    /// by the decode thread, which hands out planes whatever was asked when
    /// its decoder exports nothing.
    kind: AtomicU8,
    backing: OnceLock<sys::Backing>,
    /// The device slots, for a queue of the handle kind.
    device: sys::DeviceSlots,
    /// Slots the consumer holds: the two-held rule is enforced here, before
    /// the ring is asked.
    held: AtomicUsize,
}

impl core::fmt::Debug for Frames {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frames")
            .field("pitch", &self.pitch)
            .field("rows", &self.rows)
            .field("kind", &self.kind())
            .field("backed", &self.backed())
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
    /// The layout lent by `planes_for` or `device_planes_for`, published
    /// with the picture.
    pitch: usize,
    uv_offset: usize,
    v_offset: usize,
    handle: Option<Handle>,
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
    /// Null for a device slot, whose planes are offsets into its handle.
    pub y: *const u8,
    pub uv: *const u8,
    /// Null for a two-plane layout.
    pub v: *const u8,
    pub pitch: usize,
    /// The device slot's descriptor, or `None` for a host slot.
    pub handle: Option<Handle>,
}

/// The kind a queue's word says.
fn kind_of(word: u8) -> FrameKind {
    if word == 0 {
        FrameKind::Planes
    } else {
        FrameKind::Handle
    }
}

fn word_of(kind: FrameKind) -> u8 {
    match kind {
        FrameKind::Planes => 0,
        FrameKind::Handle => 1,
    }
}

impl Frames {
    /// A queue of `kind` for pictures up to `ceiling`.
    pub fn new(ceiling: (u32, u32), kind: FrameKind) -> Self {
        let width = usize::try_from(ceiling.0.max(16)).unwrap_or(16);
        // Two bytes a sample at the deepest layout, rows aligned to a cache
        // line.
        let pitch = (width * 2).div_ceil(HOST_ALIGN) * HOST_ALIGN;
        Self {
            ring: Latest::new(Frame::BLANK),
            pitch,
            rows: ceiling.1.max(16),
            kind: AtomicU8::new(word_of(kind)),
            backing: OnceLock::new(),
            device: sys::DeviceSlots::new(),
            held: AtomicUsize::new(0),
        }
    }

    /// The kind the session asks for.
    pub fn kind(&self) -> FrameKind {
        // Relaxed: a word the decode thread reads per picture and acts on at
        // the next one; nothing else is published through it.
        kind_of(self.kind.load(Ordering::Relaxed))
    }

    /// Ask for `kind` from the next picture on. A slot already published or
    /// held keeps the backing it was made with.
    pub fn set_kind(&self, kind: FrameKind) {
        self.kind.store(word_of(kind), Ordering::Relaxed);
    }

    fn slot_bytes(&self) -> usize {
        // Three planes of the picture's size: full chroma at the deepest.
        let rows = usize::try_from(self.rows).unwrap_or(16);
        self.pitch * rows * 3
    }

    /// Whether any slot is backed yet.
    pub fn backed(&self) -> bool {
        self.backing.get().is_some() || self.device.backed()
    }

    /// Bytes the host slots reserve once the first picture is laid out in
    /// them, backed or not; zero while the session asks for device slots,
    /// which are allocated at the stream's size as it comes.
    pub fn reserve_bytes(&self) -> usize {
        match self.kind() {
            FrameKind::Planes => self.slot_bytes() * SLOTS,
            FrameKind::Handle => 0,
        }
    }

    fn backing(&self) -> &sys::Backing {
        self.backing
            .get_or_init(|| sys::Backing::new(self.slot_bytes() * SLOTS))
    }

    fn slot_ptr(&self, index: usize) -> *mut u8 {
        let backing = self.backing();
        let offset = (index % SLOTS) * self.slot_bytes();
        // SAFETY: `offset` is inside the allocation for every slot index.
        unsafe { backing.ptr().add(offset) }
    }

    /// The producer takes a slot to decode into. `None` only if every slot
    /// is held or filling, which the two-held rule prevents.
    pub fn fill(&self) -> Option<Filling<'_>> {
        let index = self.ring.begin()?;
        Some(Filling {
            frames: self,
            index,
            published: false,
            pitch: 0,
            uv_offset: 0,
            v_offset: 0,
            handle: None,
        })
    }

    /// The consumer takes the newest picture after `after`, waiting up to
    /// `timeout`. `Err` when it already holds the most it may.
    pub fn acquire(&self, after: u64, timeout: Duration) -> Result<Option<Held>, TooManyHeld> {
        if self.held.load(Ordering::Acquire) >= MAX_HELD {
            return Err(TooManyHeld);
        }
        // Through the platform's gate: a picture whose device work is not
        // yet known finished is not handed out, and the wait for one sleeps
        // on its device's progress.
        let Some(Taken {
            index,
            seq,
            payload,
        }) = self.device.acquire(&self.ring, after, timeout)
        else {
            return Ok(None);
        };
        self.held.fetch_add(1, Ordering::AcqRel);
        let (y, uv, v) = if payload.handle.is_some() {
            // A device slot: the planes are offsets into the handle.
            (core::ptr::null(), core::ptr::null(), core::ptr::null())
        } else {
            let y = self.slot_ptr(index).cast_const();
            // SAFETY: the offsets are the layout `publish` wrote from
            // `planes_for`, which kept them inside the slot.
            let uv = unsafe { y.add(payload.uv_offset) };
            let v = if payload.format.full_chroma() {
                // SAFETY: as above.
                unsafe { y.add(payload.v_offset) }
            } else {
                core::ptr::null()
            };
            (y, uv, v)
        };
        Ok(Some(Held {
            index,
            seq,
            frame: payload,
            y,
            uv,
            v,
            pitch: payload.pitch,
            handle: payload.handle,
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

    /// No more pictures are coming: wake every waiter, one sleeping on a
    /// device's progress included.
    pub fn close(&self) {
        self.ring.close();
        self.device.wake();
    }

    /// Whether `close` was called.
    pub fn closed(&self) -> bool {
        self.ring.closed()
    }

    /// Pictures are coming again, from the next session's decode thread: the
    /// queue takes waiters, and a picture the last session left untaken is
    /// let go. Only with no decode thread running.
    pub fn reopen(&self) {
        self.ring.reopen();
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
    /// The planes to decode a `width` x `height` picture of `format` into:
    /// rows of the picture's own width, aligned to a cache line, the chroma
    /// planes straight after the luma rows. `None` if the picture does not
    /// fit the slot -- refused whole, never truncated. Lent whatever kind the
    /// session asks for: a decoder that exports nothing hands out planes.
    pub fn planes_for(&mut self, width: u32, height: u32, format: Format) -> Option<Planes<'_>> {
        let layout = Layout::of(width, height, format, HOST_ALIGN)?;
        if layout.bytes > self.frames.slot_bytes() {
            return None;
        }
        let base = self.frames.slot_ptr(self.index);
        // Usable before anything is lent over it; a platform that cannot
        // make the picture's bytes usable refuses the picture whole.
        if !self
            .frames
            .backing()
            .commit(self.index % SLOTS, base, layout.bytes)
        {
            return None;
        }
        self.pitch = layout.pitch;
        self.uv_offset = layout.uv_offset;
        self.v_offset = layout.v_offset;
        let luma = layout.uv_offset;
        let chroma = if format.full_chroma() {
            layout.v_offset - layout.uv_offset
        } else {
            layout.bytes - layout.uv_offset
        };
        // SAFETY: the slot is lent to this producer alone until it is
        // published or abandoned; the ranges are disjoint and inside the
        // slot, checked above.
        let (y, uv, v) = unsafe {
            (
                core::slice::from_raw_parts_mut(base, luma),
                core::slice::from_raw_parts_mut(base.add(luma), chroma),
                core::slice::from_raw_parts_mut(
                    base.add(luma + chroma),
                    if format.full_chroma() { chroma } else { 0 },
                ),
            )
        };
        Some(Planes {
            y,
            y_pitch: layout.pitch,
            uv,
            uv_pitch: layout.pitch,
            v,
            v_pitch: layout.pitch,
        })
    }

    /// The picture is in the slot: publish it as the newest, with the layout
    /// it was decoded into.
    pub fn publish(self, frame: Frame) {
        self.publish_gated(frame, 0);
    }

    /// As [`publish`](Self::publish), for a picture whose device work is
    /// finished once the slot's device reaches `value`; zero for a picture
    /// finished now. It is not handed out before.
    pub fn publish_gated(mut self, mut frame: Frame, value: u64) {
        frame.pitch = self.pitch;
        frame.uv_offset = self.uv_offset;
        frame.v_offset = self.v_offset;
        frame.handle = self.handle;
        self.frames.ring.set(self.index, frame);
        self.frames
            .ring
            .publish_gated(self.index, self.frames.device.gate(value));
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
        let frames = Frames::new((4096, 4096), FrameKind::Planes);
        assert!(!frames.backed());
        assert_eq!(frames.reserve_bytes(), 4 * (8192 * 4096 * 3));
        let mut filling = frames.fill().unwrap();
        let planes = filling.planes_for(1920, 1080, Format::Nv12).unwrap();
        assert_eq!(planes.y_pitch, 1920);
        assert!(frames.backed());
    }

    /// The planes are the picture's own size: what a picture touches in a
    /// slot at the ceiling is the picture, not the ceiling.
    #[test]
    fn the_planes_are_laid_out_at_the_pictures_own_pitch() {
        let frames = Frames::new((4096, 4096), FrameKind::Planes);
        let mut filling = frames.fill().unwrap();
        {
            let planes = filling.planes_for(64, 64, Format::Nv12).unwrap();
            assert_eq!(
                (planes.y_pitch, planes.y.len(), planes.uv.len()),
                (64, 64 * 64, 64 * 32)
            );
        }
        {
            let planes = filling.planes_for(1366, 768, Format::Nv12).unwrap();
            assert_eq!(planes.y_pitch, 1408, "rows are aligned to a cache line");
        }
        {
            let planes = filling.planes_for(1920, 1080, Format::P010).unwrap();
            assert_eq!(
                (planes.y_pitch, planes.uv.len(), planes.v.len()),
                (3840, 3840 * 540, 0)
            );
        }
        {
            let planes = filling.planes_for(1920, 1080, Format::Yuv444_16).unwrap();
            assert_eq!(
                (planes.y_pitch, planes.uv.len(), planes.v.len()),
                (3840, 3840 * 1080, 3840 * 1080)
            );
        }
        assert!(
            filling.planes_for(4096, 4098, Format::Yuv444_16).is_none(),
            "a picture past the slot is refused, not truncated"
        );
        assert!(filling.planes_for(0, 16, Format::Nv12).is_none());
    }

    /// A slot the application holds keeps the layout it was published with
    /// while smaller pictures are decoded and handed out after it.
    #[test]
    fn a_held_slot_keeps_its_layout_across_a_smaller_picture() {
        let frames = Frames::new((4096, 4096), FrameKind::Planes);
        let mut filling = frames.fill().unwrap();
        {
            let planes = filling.planes_for(1920, 1080, Format::Nv12).unwrap();
            planes.uv[0] = 0x11;
        }
        filling.publish(Frame {
            width: 1920,
            height: 1080,
            ..Frame::BLANK
        });
        let big = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        assert_eq!((big.pitch, big.frame.uv_offset), (1920, 1920 * 1080));

        let mut filling = frames.fill().unwrap();
        {
            let planes = filling.planes_for(64, 64, Format::Nv12).unwrap();
            planes.uv[0] = 0x22;
        }
        filling.publish(frame(2));
        let small = frames.acquire(big.seq, Duration::ZERO).unwrap().unwrap();
        assert_eq!((small.pitch, small.frame.uv_offset), (64, 64 * 64));
        // SAFETY: both slots are held.
        unsafe {
            assert_eq!(*big.uv, 0x11);
            assert_eq!(*small.uv, 0x22);
        }
    }

    #[test]
    fn a_third_hold_is_refused_and_a_release_makes_room() {
        let frames = Frames::new((64, 64), FrameKind::Planes);
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
        let frames = Frames::new((64, 64), FrameKind::Planes);
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
        let frames = Frames::new((64, 64), FrameKind::Planes);
        {
            let mut filling = frames.fill().unwrap();
            let _ = filling.planes_for(64, 64, Format::Nv12);
        }
        assert_eq!(frames.ready(), 0);
        assert!(frames.acquire(0, Duration::ZERO).unwrap().is_none());
    }

    #[test]
    fn what_is_written_is_what_is_read() {
        let frames = Frames::new((64, 64), FrameKind::Planes);
        let mut filling = frames.fill().unwrap();
        {
            let planes = filling.planes_for(64, 64, Format::Nv12).unwrap();
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

    /// A queue asking for handles backs nothing before a picture, and no
    /// device memory until a backing is attached; a picture a decoder hands
    /// out as planes all the same is laid out in host memory as ever.
    #[test]
    fn a_handle_queue_backs_nothing_until_a_picture_needs_it() {
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        assert_eq!(frames.reserve_bytes(), 0);
        let mut filling = frames.fill().unwrap();
        #[cfg(target_os = "linux")]
        assert!(filling.device_planes_for(64, 64, Format::Nv12).is_none());
        #[cfg(windows)]
        assert!(filling.textures_for(64, 64, Format::Nv12).is_none());
        assert!(!frames.backed());
        assert!(
            filling.planes_for(64, 64, Format::Nv12).is_some(),
            "planes refused to a decoder that exports nothing"
        );
        assert!(frames.backed());
    }

    /// The kind asked for switches at once, and a picture already held keeps
    /// the layout it was published with.
    #[test]
    fn the_kind_switches_and_a_held_picture_keeps_its_own() {
        let frames = Frames::new((64, 64), FrameKind::Planes);
        let mut filling = frames.fill().unwrap();
        let _ = filling.planes_for(64, 64, Format::Nv12).unwrap();
        filling.publish(frame(1));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        frames.set_kind(FrameKind::Handle);
        assert_eq!(frames.kind(), FrameKind::Handle);
        assert!(
            held.handle.is_none() && !held.y.is_null(),
            "the held picture changed"
        );
        frames.set_kind(FrameKind::Planes);
        assert_eq!(frames.kind(), FrameKind::Planes);
    }
}
