//! The queue's platform half on Linux: host slots in one zeroed allocation,
//! and device slots made through the vendor's compute runtime, each exported
//! as a descriptor the application imports.

use std::os::fd::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex};

use lowlat_decode::Format;
use lowlat_decode::nvdec::DevicePlanes;
use lowlat_drivers::cuda::{self, Cuda, Exportable};

use super::{Filling, FrameKind, Frames, Layout, SLOTS};

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
    device: cuda::Device,
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

/// The device slots of a queue of the handle kind: touched by the producer
/// alone, under a lock only so the drop may happen anywhere.
pub(super) struct DeviceSlots(Mutex<Option<DeviceBacking>>);

impl DeviceSlots {
    pub(super) fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Whether any device slot is allocated yet.
    pub(super) fn backed(&self) -> bool {
        self.0
            .lock()
            .is_ok_and(|d| d.as_ref().is_some_and(|d| d.allocations > 0))
    }
}

impl Frames {
    /// Attach the runtime the device slots are made through. Called on
    /// the decode thread before its first picture, for a queue of the
    /// handle kind; the slots themselves are allocated as pictures come.
    pub fn open_device(&self, cuda: Arc<Cuda>, device: cuda::Device) {
        if let Ok(mut guard) = self.device.0.lock() {
            *guard = Some(DeviceBacking {
                slots: [const { None }; SLOTS],
                allocations: 0,
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
        if self.frames.kind != FrameKind::Handle {
            return None;
        }
        let layout = Layout::of(width, height, format, DEVICE_ALIGN)?;
        let mut guard = self.frames.device.0.lock().ok()?;
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
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{Frame, SLOTS};
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
