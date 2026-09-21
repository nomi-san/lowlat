//! The decoded-picture queue: four slots between the decode thread and the
//! application (docs/10-client.md section 4).
//!
//! The ordering is the common ring's; what is here is the storage, of one
//! of two kinds settled at creation. **Host slots** are sized once, from
//! the configuration's ceiling at the deepest layout a decoder here
//! produces (full chroma at sixteen bits: three planes of two-byte
//! samples), and backed on the decode thread at the first decoder build,
//! demand-zero: nothing is allocated at creation or at an attempt, and the
//! working set is the pictures actually written, never the reserve. A
//! rebuild never reallocates, so a slot the application holds is never
//! pulled from under it.
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

use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use lowlat_common::latest::{Latest, Taken};
use lowlat_core::video::Rotation;
use lowlat_decode::nvdec::DevicePlanes;
use lowlat_decode::{Format, Planes};
use lowlat_drivers::cuda::{Cuda, Device, Exportable};

use crate::config::FrameKind;

/// Slots: two the application may hold, one being decoded into, one ready.
pub const SLOTS: usize = 4;
/// The most the application may hold at once.
pub const MAX_HELD: usize = 2;
/// Rows of a host slot are aligned to a cache line.
const HOST_ALIGN: usize = 64;
/// Rows of a device slot are aligned as the device's own surfaces are.
const DEVICE_ALIGN: usize = 256;

/// The descriptor behind a device slot, as the application is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle {
    /// The library's for the lease: closed when the allocation is freed,
    /// which is after the last hold on it is released. An import that
    /// takes ownership of a descriptor is given a duplicate.
    pub fd: RawFd,
    /// The whole allocation, which is what an import is told.
    pub size: usize,
    /// The allocation's ordinal since creation, from one. Descriptor
    /// numbers are reused once closed, so this is what tells one
    /// allocation from the next: two frames with the same ordinal share an
    /// import, and a new ordinal is a new import.
    pub allocation: u32,
}

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

/// The one host allocation, made on the decode thread.
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

/// One device slot: its allocation and what it was laid out for.
struct DeviceSlot {
    memory: Exportable,
    layout: Layout,
    allocation: u32,
}

/// The device slots and the runtime they are made through, attached on
/// the decode thread before its first picture.
struct DeviceBacking {
    slots: [Option<DeviceSlot>; SLOTS],
    /// Ordinals handed out so far.
    allocations: u32,
    device: Device,
    /// Last, so the allocations' entry points outlive them.
    cuda: Arc<Cuda>,
}

impl DeviceBacking {
    /// The slot at `index`, allocated for `layout`: the existing allocation
    /// if it was made for exactly this layout, else a fresh one in its
    /// place. The previous one drops here, which is safe because the ring
    /// lent this slot to the producer, so nothing holds it.
    fn slot_for(&mut self, index: usize, layout: Layout) -> Option<&DeviceSlot> {
        let slot = self.slots.get_mut(index)?;
        if slot.as_ref().is_none_or(|s| s.layout != layout) {
            let memory = self
                .cuda
                .alloc_exportable(&self.device, layout.bytes)
                .ok()?;
            self.allocations += 1;
            *slot = Some(DeviceSlot {
                memory,
                layout,
                allocation: self.allocations,
            });
        }
        slot.as_ref()
    }
}

/// The queue.
pub struct Frames {
    ring: Latest<Frame, SLOTS>,
    /// Bytes per row, for the widest sample at the ceiling, aligned.
    pitch: usize,
    /// Luma rows at the ceiling.
    rows: u32,
    /// Which kind of slot this queue lends.
    kind: FrameKind,
    backing: OnceLock<Backing>,
    /// The device slots, for a queue of the handle kind; touched by the
    /// producer alone, under a lock only so the drop may happen anywhere.
    device: Mutex<Option<DeviceBacking>>,
    /// Slots the consumer holds: the two-held rule is enforced here, before
    /// the ring is asked.
    held: AtomicUsize,
}

impl core::fmt::Debug for Frames {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frames")
            .field("pitch", &self.pitch)
            .field("rows", &self.rows)
            .field("kind", &self.kind)
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
            kind,
            backing: OnceLock::new(),
            device: Mutex::new(None),
            held: AtomicUsize::new(0),
        }
    }

    /// Which kind of slot this queue lends.
    pub fn kind(&self) -> FrameKind {
        self.kind
    }

    /// Attach the runtime the device slots are made through. Called on
    /// the decode thread before its first picture, for a queue of the
    /// handle kind; the slots themselves are allocated as pictures come.
    pub fn open_device(&self, cuda: Arc<Cuda>, device: Device) {
        if let Ok(mut guard) = self.device.lock() {
            *guard = Some(DeviceBacking {
                slots: [const { None }; SLOTS],
                allocations: 0,
                device,
                cuda,
            });
        }
    }

    fn slot_bytes(&self) -> usize {
        // Three planes of the picture's size: full chroma at the deepest.
        let rows = usize::try_from(self.rows).unwrap_or(16);
        self.pitch * rows * 3
    }

    /// Whether any slot is backed yet.
    pub fn backed(&self) -> bool {
        self.backing.get().is_some()
            || self
                .device
                .lock()
                .is_ok_and(|d| d.as_ref().is_some_and(|d| d.allocations > 0))
    }

    /// Bytes the host slots reserve, backed or not; zero for device slots,
    /// which are allocated at the stream's size as it comes.
    pub fn reserve_bytes(&self) -> usize {
        match self.kind {
            FrameKind::Planes => self.slot_bytes() * SLOTS,
            FrameKind::Handle => 0,
        }
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
        let Some(Taken {
            index,
            seq,
            payload,
        }) = self.ring.acquire(after, timeout)
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

    /// No more pictures are coming: wake every waiter.
    pub fn close(&self) {
        self.ring.close();
    }

    /// Whether `close` was called.
    pub fn closed(&self) -> bool {
        self.ring.closed()
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
    /// fit the slot -- refused whole, never truncated.
    pub fn planes_for(&mut self, width: u32, height: u32, format: Format) -> Option<Planes<'_>> {
        if self.frames.kind != FrameKind::Planes {
            return None;
        }
        let layout = Layout::of(width, height, format, HOST_ALIGN)?;
        if layout.bytes > self.frames.slot_bytes() {
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
        let base = self.frames.slot_ptr(self.index);
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

    /// The device planes to decode a `width` x `height` picture of
    /// `format` into, on a queue of the handle kind with its runtime
    /// attached: the slot's allocation, made or remade for this layout.
    /// `None` when there is no runtime or the device refused.
    pub fn device_planes_for(
        &mut self,
        width: u32,
        height: u32,
        format: Format,
    ) -> Option<DevicePlanes> {
        if self.frames.kind != FrameKind::Handle {
            return None;
        }
        let layout = Layout::of(width, height, format, DEVICE_ALIGN)?;
        let mut guard = self.frames.device.lock().ok()?;
        let slot = guard.as_mut()?.slot_for(self.index, layout)?;
        self.pitch = layout.pitch;
        self.uv_offset = layout.uv_offset;
        self.v_offset = layout.v_offset;
        self.handle = Some(Handle {
            fd: slot.memory.fd().as_raw_fd(),
            size: slot.memory.size(),
            allocation: slot.allocation,
        });
        let base = slot.memory.ptr();
        let at = |offset: usize| base + u64::try_from(offset).unwrap_or(0);
        Some(DevicePlanes {
            y: base,
            y_pitch: layout.pitch,
            uv: at(layout.uv_offset),
            uv_pitch: layout.pitch,
            v: if format.full_chroma() {
                at(layout.v_offset)
            } else {
                0
            },
            v_pitch: layout.pitch,
        })
    }

    /// The picture is in the slot: publish it as the newest, with the layout
    /// it was decoded into.
    pub fn publish(mut self, mut frame: Frame) {
        frame.pitch = self.pitch;
        frame.uv_offset = self.uv_offset;
        frame.v_offset = self.v_offset;
        frame.handle = self.handle;
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

    /// A queue of the handle kind lends no host memory, and no device
    /// memory until its runtime is attached.
    #[test]
    fn a_handle_queue_backs_no_host_memory() {
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        assert_eq!(frames.reserve_bytes(), 0);
        let mut filling = frames.fill().unwrap();
        assert!(filling.planes_for(64, 64, Format::Nv12).is_none());
        assert!(filling.device_planes_for(64, 64, Format::Nv12).is_none());
        assert!(!frames.backed());
    }

    /// The runtime, on the first device: what the device tests run on.
    /// `None` without the vendor driver, and the test says so and passes.
    fn device_queue(ceiling: (u32, u32)) -> Option<Frames> {
        let cuda = Cuda::load().ok()?;
        let device = cuda.any_device().ok()?;
        let frames = Frames::new(ceiling, FrameKind::Handle);
        frames.open_device(Arc::new(cuda), device);
        Some(frames)
    }

    /// Whether a descriptor number is open: a duplicate succeeds only
    /// then, and is closed again at once.
    fn descriptor_is_open(fd: RawFd) -> bool {
        // SAFETY: the number is only borrowed for a duplicate, which fails
        // harmlessly on a closed one.
        unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .is_ok()
    }

    /// Device slots are made at the picture's layout, exported, and laid
    /// out at the device alignment; a picture of the same layout reuses
    /// the slot's allocation, so the descriptor an application imported
    /// stays the one it sees.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn a_device_slot_is_allocated_once_per_layout() {
        let Some(frames) = device_queue((4096, 4096)) else {
            println!("no vendor runtime; not exercised");
            return;
        };
        let mut filling = frames.fill().unwrap();
        let planes = filling.device_planes_for(1366, 768, Format::Nv12).unwrap();
        assert_eq!(planes.y_pitch, 1536, "rows at the device alignment");
        assert_eq!(planes.uv, planes.y + 1536 * 768);
        assert_eq!(planes.v, 0);
        filling.publish(frame(1));
        let first = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let handle = first.handle.expect("a device slot carries its handle");
        assert!(first.y.is_null() && first.uv.is_null());
        assert_eq!((first.pitch, first.frame.uv_offset), (1536, 1536 * 768));
        assert!(handle.size >= 1536 * 768 * 3 / 2);
        assert!(descriptor_is_open(handle.fd));
        frames.release(first.index);

        // Many more pictures at the same layout: every slot keeps the one
        // allocation it was given, whichever order the ring lends them in.
        let mut by_slot = [None; SLOTS];
        by_slot[first.index] = Some(handle);
        let mut last = first.seq;
        for n in 2..20 {
            let mut filling = frames.fill().unwrap();
            let _ = filling.device_planes_for(1366, 768, Format::Nv12).unwrap();
            filling.publish(frame(n));
            let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
            last = h.seq;
            let seen = by_slot[h.index].get_or_insert(h.handle.unwrap());
            assert_eq!(*seen, h.handle.unwrap(), "slot {} reallocated", h.index);
            frames.release(h.index);
        }
        let ordinals: Vec<u32> = by_slot.iter().flatten().map(|h| h.allocation).collect();
        assert!(ordinals.iter().all(|o| *o <= 4), "{ordinals:?}");
    }

    /// A slot the application holds keeps its allocation while pictures
    /// of a new layout are decoded into the other slots; once released
    /// and refilled it gets a fresh allocation, with a new ordinal even
    /// though the descriptor number may repeat.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn a_held_device_slot_outlives_a_resize() {
        let Some(frames) = device_queue((4096, 4096)) else {
            println!("no vendor runtime; not exercised");
            return;
        };
        let mut filling = frames.fill().unwrap();
        let _ = filling.device_planes_for(1920, 1080, Format::P010).unwrap();
        filling.publish(frame(1));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let old = held.handle.unwrap();

        // The stream changes size: the next three slots are remade.
        let mut last = held.seq;
        let mut ordinals = Vec::new();
        for n in 2..5 {
            let mut filling = frames.fill().unwrap();
            assert_ne!(filling.index, held.index, "the held slot was lent");
            let _ = filling.device_planes_for(1280, 720, Format::Nv12).unwrap();
            filling.publish(frame(n));
            let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
            last = h.seq;
            ordinals.push(h.handle.unwrap().allocation);
            frames.release(h.index);
        }
        assert!(ordinals.iter().all(|o| *o > old.allocation));
        assert!(
            descriptor_is_open(old.fd),
            "the held slot's descriptor was closed under it"
        );
        frames.release(held.index);

        // Refilled at the new layout, the slot gets a fresh allocation.
        let mut filling = frames.fill().unwrap();
        let _ = filling.device_planes_for(1280, 720, Format::Nv12).unwrap();
        filling.publish(frame(5));
        let fresh = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
        let new = fresh.handle.unwrap();
        assert!(new.allocation > *ordinals.iter().max().unwrap());
        assert_ne!(new, old);
        frames.release(fresh.index);
    }

    /// The two-held rule is the ring's, whatever backs the slots.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn a_third_device_hold_is_refused() {
        let Some(frames) = device_queue((256, 256)) else {
            println!("no vendor runtime; not exercised");
            return;
        };
        let mut last = 0;
        for n in 0..3 {
            let mut filling = frames.fill().unwrap();
            let _ = filling.device_planes_for(64, 64, Format::Nv12).unwrap();
            filling.publish(frame(n));
            if n < 2 {
                last = frames.acquire(last, Duration::ZERO).unwrap().unwrap().seq;
            }
        }
        assert!(
            frames.acquire(last, Duration::ZERO).is_err(),
            "a third hold"
        );
    }
}
