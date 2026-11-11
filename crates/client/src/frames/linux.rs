//! The queue's platform half on Linux: host slots in one zeroed allocation,
//! device slots made through the vendor's compute runtime, each exported as
//! a descriptor the application imports, and the open stack's own surfaces,
//! lent to the slot their picture is published in.
//!
//! **A device slot carries its own backing**: the allocation and the
//! backing generation it was made in. The session's backing moves on when
//! the vendor's decoder opens on another device, and a slot is made again
//! the next time the ring lends it to the producer, never while the
//! application holds it. Every picture is finished when it is published --
//! the device copy is waited for -- so nothing here is gated.
//!
//! **A lent surface is held by its slot** from its picture's publish until
//! the slot is lent to the producer again -- never while the application
//! holds it -- through a duplicate of its descriptor, so the buffer outlives
//! a decoder torn down meanwhile. Its decode is waited for before it is
//! published, and the decoder writes it again only after the slot lets it
//! go, so nothing here is gated either.

use core::time::Duration;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};

use lowlat_common::latest::{Latest, Taken};
use lowlat_decode::nvdec::DevicePlanes;
use lowlat_decode::vaapi::Lent;
use lowlat_decode::{Format, Lease};
use lowlat_drivers::cuda::{self, Cuda, Exportable};

use super::{Filling, Frame, Frames, Layout, SLOTS};

/// Rows of a device slot are aligned as the device's own surfaces are.
const DEVICE_ALIGN: usize = 256;

/// What a handle's descriptor is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// A device slot of the vendor's compute runtime, its rows laid out
    /// plainly at each plane's offset and pitch.
    Opaque,
    /// The open stack's own surface, a kernel buffer laid out as `modifier`
    /// says.
    DmaBuf,
}

/// The descriptor behind a device slot or a lent surface, as the
/// application is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle {
    pub kind: HandleKind,
    /// The library's for the lease: closed when the allocation is freed,
    /// or when the slot lets its lent surface go, which is after the last
    /// hold on it is released. An import that takes ownership of a
    /// descriptor is given a duplicate.
    pub fd: RawFd,
    /// The whole allocation, which is what an import is told.
    pub size: usize,
    /// The allocation's ordinal since creation, from one. Descriptor
    /// numbers are reused once closed, so this is what tells one
    /// allocation from the next: two frames with the same ordinal share an
    /// import, and a new ordinal is a new import.
    pub allocation: u32,
    /// The buffer's layout, for a lent surface; zero otherwise.
    pub modifier: u64,
    /// The render node the lent surface is on, major in the high half; zero
    /// otherwise.
    pub device: u64,
}

/// The one host allocation, made on the decode thread.
pub(super) struct Backing {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the bytes are written only inside a slot the ring has lent the
// producer and read only inside one it has lent the consumer, and the ring's
// state transitions carry the ordering. The pointer itself is set once,
// before the first publish, through the lock.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}

impl Backing {
    /// Demand-zero: the pages are mapped when a picture is first written
    /// into them, never here.
    pub(super) fn new(len: usize) -> Self {
        let boxed = vec![0u8; len].into_boxed_slice();
        let ptr = Box::into_raw(boxed).cast::<u8>();
        Self { ptr, len }
    }

    pub(super) fn ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Nothing to do: the system charges a page when it is first written,
    /// and nothing before.
    pub(super) fn commit(&self, _slot: usize, _start: *mut u8, _bytes: usize) -> bool {
        true
    }
}

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

/// One device slot: its allocation and what it was made for. The runtime
/// it came from stays with it, so an allocation of an older backing is
/// freed through the runtime that made it.
struct DeviceSlot {
    memory: Exportable,
    layout: Layout,
    generation: u32,
    allocation: u32,
    /// Last, so the allocation's entry points outlive it.
    _cuda: Arc<Cuda>,
}

/// The session's backing now: the device and the runtime slots are made
/// through.
struct Current {
    generation: u32,
    device: cuda::Device,
    cuda: Arc<Cuda>,
}

/// A lent surface a slot holds: the duplicate that keeps its buffer alive,
/// and what the decoder that lent it is told when the slot lets it go.
struct Held {
    _fd: OwnedFd,
    lease: Lease,
}

/// The device slots and the backing they are made on.
struct DeviceBacking {
    slots: [Option<DeviceSlot>; SLOTS],
    /// The lent surface each slot holds, if any.
    lent: [Option<Held>; SLOTS],
    /// Ordinals handed out so far, to device slots and lent surfaces alike.
    allocations: u32,
    /// Backings attached so far.
    generations: u32,
    current: Option<Current>,
}

impl DeviceBacking {
    /// The slot at `index`, allocated for `layout` on the current backing:
    /// the existing allocation if it was made for exactly this layout on
    /// this backing, else a fresh one in its place. The previous one drops
    /// here, which is safe because the ring lent this slot to the producer,
    /// so nothing holds it.
    fn slot_for(&mut self, index: usize, layout: Layout) -> Option<&DeviceSlot> {
        let current = self.current.as_ref()?;
        let slot = self.slots.get_mut(index)?;
        if slot
            .as_ref()
            .is_none_or(|s| s.layout != layout || s.generation != current.generation)
        {
            *slot = None;
            let memory = current
                .cuda
                .alloc_exportable(&current.device, layout.bytes)
                .ok()?;
            self.allocations += 1;
            *slot = Some(DeviceSlot {
                memory,
                layout,
                generation: current.generation,
                allocation: self.allocations,
                _cuda: Arc::clone(&current.cuda),
            });
        }
        slot.as_ref()
    }
}

/// The device slots of a queue: touched by the producer alone, under a lock
/// only so the drop may happen anywhere.
pub(super) struct DeviceSlots(Mutex<DeviceBacking>);

impl DeviceSlots {
    pub(super) fn new() -> Self {
        Self(Mutex::new(DeviceBacking {
            slots: [const { None }; SLOTS],
            lent: [const { None }; SLOTS],
            allocations: 0,
            generations: 0,
            current: None,
        }))
    }

    /// Whether any device slot is allocated yet.
    pub(super) fn backed(&self) -> bool {
        self.0.lock().is_ok_and(|d| d.allocations > 0)
    }

    /// The gate a picture is published behind: every picture here is
    /// finished when it is published.
    pub(super) fn gate(&self, value: u64) -> u64 {
        let _ = value;
        0
    }

    /// The newest picture, waiting up to `timeout` for one.
    pub(super) fn acquire(
        &self,
        ring: &Latest<Frame, SLOTS>,
        after: u64,
        timeout: Duration,
    ) -> Option<Taken<Frame>> {
        ring.acquire(after, timeout)
    }

    /// Nothing sleeps here but on the ring, which the close wakes itself.
    pub(super) fn wake(&self) {}

    /// The lent surface slot `index` held, let go now that the ring has lent
    /// the slot to the producer: its duplicate closed, and the lease for the
    /// decoder that lent it.
    pub(super) fn end_lease(&self, index: usize) -> Option<Lease> {
        let held = self.0.lock().ok()?.lent.get_mut(index)?.take()?;
        Some(held.lease)
    }
}

impl Frames {
    /// The session's backing moves to `device`, through `cuda`. Called on
    /// the decode thread each time it opens the vendor's decoder; a slot of
    /// an older backing is made again when it is next lent, and one the
    /// application holds keeps its allocation until then.
    pub fn open_device(&self, cuda: Arc<Cuda>, device: cuda::Device) {
        if let Ok(mut backing) = self.device.0.lock() {
            backing.generations += 1;
            backing.current = Some(Current {
                generation: backing.generations,
                device,
                cuda,
            });
        }
    }
}

impl Filling<'_> {
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
        let layout = Layout::of(width, height, format, DEVICE_ALIGN)?;
        let mut guard = self.frames.device.0.lock().ok()?;
        let slot = guard.slot_for(self.index, layout)?;
        self.pitch = layout.pitch;
        self.uv_pitch = layout.uv_pitch;
        self.uv_offset = layout.uv_offset;
        self.v_offset = layout.v_offset;
        self.handle = Some(Handle {
            kind: HandleKind::Opaque,
            fd: slot.memory.fd().as_raw_fd(),
            size: slot.memory.size(),
            allocation: slot.allocation,
            modifier: 0,
            device: 0,
        });
        let base = slot.memory.ptr();
        let at = |offset: usize| base + u64::try_from(offset).unwrap_or(0);
        Some(DevicePlanes {
            y: base,
            y_pitch: layout.pitch,
            uv: at(layout.uv_offset),
            uv_pitch: layout.uv_pitch,
            v: if format.planar_chroma() {
                at(layout.v_offset)
            } else {
                0
            },
            v_pitch: layout.uv_pitch,
        })
    }

    /// An ordinal for an allocation made for the first time: device slots
    /// and lent surfaces count together, so no two of either share one.
    pub fn new_allocation(&self) -> u32 {
        self.frames.device.0.lock().map_or(0, |mut backing| {
            backing.allocations += 1;
            backing.allocations
        })
    }

    /// The picture is a lent surface, held by this slot until the ring
    /// lends the slot to the producer again; published with the surface's
    /// layout as its handle. A lease the slot still held, which the
    /// producer's [`take_lease`](Self::take_lease) leaves none of, is
    /// returned for its decoder.
    pub fn lend_surface(&mut self, lent: Lent) -> Option<Lease> {
        self.pitch = usize::try_from(lent.pitches[0]).unwrap_or(0);
        self.uv_pitch = usize::try_from(lent.pitches[1]).unwrap_or(0);
        self.uv_offset = usize::try_from(lent.chroma_offset).unwrap_or(0);
        self.v_offset = 0;
        self.handle = Some(Handle {
            kind: HandleKind::DmaBuf,
            fd: lent.fd.as_raw_fd(),
            size: usize::try_from(lent.size).unwrap_or(0),
            allocation: lent.allocation,
            modifier: lent.modifier,
            device: lent.device,
        });
        let held = Held {
            _fd: lent.fd,
            lease: lent.lease,
        };
        let mut backing = self.frames.device.0.lock().ok()?;
        let slot = backing.lent.get_mut(self.index)?;
        slot.replace(held).map(|old| old.lease)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{Frame, FrameKind, SLOTS};
    use super::*;

    fn frame(order: i32) -> Frame {
        Frame {
            order,
            width: 64,
            height: 64,
            ..Frame::BLANK
        }
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

    /// **A held slot outlives a move to another backing.** The runtime is
    /// attached again (a device chosen mid-session): the slot the
    /// application holds keeps its allocation and its descriptor stays open;
    /// every other slot is made again on the new backing, and the held one
    /// is too once it is released and lent again.
    #[test]
    #[ignore = "requires the vendor driver"]
    fn a_held_device_slot_outlives_a_move() {
        let Some(frames) = device_queue((4096, 4096)) else {
            println!("no vendor runtime; not exercised");
            return;
        };
        let mut filling = frames.fill().unwrap();
        let _ = filling.device_planes_for(1280, 720, Format::Nv12).unwrap();
        filling.publish(frame(1));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let old = held.handle.unwrap();

        let cuda = Cuda::load().unwrap();
        let device = cuda.any_device().unwrap();
        frames.open_device(Arc::new(cuda), device);

        let mut last = held.seq;
        for n in 2..6 {
            let mut filling = frames.fill().unwrap();
            assert_ne!(filling.index, held.index, "the held slot was lent");
            let _ = filling.device_planes_for(1280, 720, Format::Nv12).unwrap();
            filling.publish(frame(n));
            let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
            last = h.seq;
            assert!(
                h.handle.unwrap().allocation > old.allocation,
                "a slot kept its old backing"
            );
            frames.release(h.index);
        }
        assert!(
            descriptor_is_open(old.fd),
            "the held slot's descriptor was closed under it"
        );
        frames.release(held.index);
    }

    /// A lent surface standing in for the decoder's: a descriptor of its
    /// own, for the slot to hold.
    fn lent(buffer: u32, allocation: u32) -> Lent {
        Lent {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            size: 4096,
            modifier: 0x0100_0000_0000_0009,
            chroma_offset: 2048,
            pitches: [64, 64],
            allocation,
            device: (226 << 32) | 129,
            lease: Lease { context: 1, buffer },
        }
    }

    /// **A lent surface is held by its slot** from its publish until the
    /// slot is lent to the producer again: published with its layout as a
    /// buffer handle, its descriptor open while the application holds the
    /// picture, and its lease handed back -- to the producer that fills the
    /// slot next, and to no other -- once the application let it go.
    #[test]
    fn a_lent_surface_is_held_until_its_slot_is_filled_again() {
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        let mut filling = frames.fill().unwrap();
        assert!(filling.take_lease().is_none(), "a fresh slot held a lease");
        let number = filling.new_allocation();
        assert!(filling.lend_surface(lent(3, number)).is_none());
        filling.publish(frame(1));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let handle = held.handle.expect("a lent surface carries its handle");
        assert_eq!(handle.kind, HandleKind::DmaBuf);
        assert_eq!(
            (
                handle.size,
                handle.allocation,
                handle.modifier,
                handle.device
            ),
            (4096, number, 0x0100_0000_0000_0009, (226 << 32) | 129)
        );
        assert_eq!(
            (held.pitch, held.uv_pitch, held.frame.uv_offset),
            (64, 64, 2048)
        );
        assert!(held.y.is_null() && held.uv.is_null());
        assert!(descriptor_is_open(handle.fd));

        // More pictures through the other slots: the held one is never lent
        // to the producer, so its lease never comes back while it is held.
        let mut ended = Vec::new();
        let mut last = held.seq;
        for n in 2..10 {
            let mut filling = frames.fill().unwrap();
            assert_ne!(filling.index, held.index, "the held slot was lent");
            ended.extend(filling.take_lease());
            assert!(
                filling
                    .lend_surface(lent(100 + n, filling.new_allocation()))
                    .is_none()
            );
            filling.publish(frame(n as i32));
            let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
            last = h.seq;
            frames.release(h.index);
        }
        assert!(
            descriptor_is_open(handle.fd),
            "closed under the application"
        );
        assert!(!ended.iter().any(|l| l.buffer == 3), "{ended:?}");
        assert!(ended.iter().all(|l| l.context == 1));

        // Let go: the slot's next filling hands its lease back, once.
        frames.release(held.index);
        let mut back = None;
        for n in 10..20 {
            let mut filling = frames.fill().unwrap();
            let lease = filling.take_lease();
            assert!(filling.take_lease().is_none(), "a lease handed back twice");
            if filling.index == held.index {
                back = lease;
                break;
            }
            filling.publish(frame(n));
            let h = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
            last = h.seq;
            frames.release(h.index);
        }
        assert_eq!(
            back,
            Some(Lease {
                context: 1,
                buffer: 3
            })
        );
    }

    /// Device slots and lent surfaces draw their numbers from one counter,
    /// so no two of either share one.
    #[test]
    fn allocations_are_numbered_once_across_both_kinds() {
        let frames = Frames::new((256, 256), FrameKind::Handle);
        let filling = frames.fill().unwrap();
        let numbers: Vec<u32> = (0..5).map(|_| filling.new_allocation()).collect();
        assert_eq!(numbers, [1, 2, 3, 4, 5]);
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
