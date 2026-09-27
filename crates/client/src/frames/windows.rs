//! The queue's platform half on Windows: host slots committed as far as the
//! pictures laid out in them reach, and no device slots yet -- the handle
//! kind comes with its step of docs/impl-plan-windows.md, and until then
//! creation refuses a queue of that kind.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lowlat_decode::Format;
use lowlat_decode::nvdec::DevicePlanes;

use super::{Filling, SLOTS};

/// A device slot's handle: there is none here yet, so there is no value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {}

// The system's memory manager is present in every process, so these need no
// crate dependency and no link attribute.
unsafe extern "system" {
    fn VirtualAlloc(address: *mut c_void, size: usize, kind: u32, protect: u32) -> *mut c_void;
    fn VirtualFree(address: *mut c_void, size: usize, kind: u32) -> i32;
}

const MEM_COMMIT: u32 = 0x0000_1000;
const MEM_RESERVE: u32 = 0x0000_2000;
const MEM_RELEASE: u32 = 0x0000_8000;
const PAGE_READWRITE: u32 = 0x04;

/// The host slots' address range, reserved on the decode thread at the first
/// picture and committed slot by slot, as far as the pictures laid out in
/// each have reached.
///
/// **The reserve is never the commit.** The system charges committed memory
/// against the machine's memory and page file whether it is ever touched or
/// not, so a zeroed allocation of the whole reserve is charged whole at once
/// -- 384.8 MiB at the ceiling, measured -- where committing as a picture
/// reaches charges the stream's own size: four pictures at 1440p are 22 MiB.
/// Commit grows and never shrinks while the queue lives, so a slot the
/// application holds is never decommitted under it.
pub(super) struct Backing {
    base: *mut u8,
    /// Bytes committed from each slot's start. The producer's alone: a slot
    /// is lent to one producer at a time, and the ring's transitions carry
    /// the order, so these are only atomics to be shared.
    committed: [AtomicUsize; SLOTS],
    /// Whether a refused commit was said, so a machine out of memory says
    /// so once rather than at every picture.
    refused: AtomicBool,
}

// SAFETY: the bytes are written only inside a slot the ring has lent the
// producer and read only inside one it has lent the consumer, and the ring's
// state transitions carry the ordering. The base is set once, before the
// first publish, through the lock.
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}

impl Backing {
    /// Reserve `len` bytes of address space and commit none of them.
    pub(super) fn new(len: usize) -> Self {
        // SAFETY: a fresh reservation wherever the system chooses; nothing
        // is committed and nothing is read.
        let base = unsafe { VirtualAlloc(core::ptr::null_mut(), len, MEM_RESERVE, PAGE_READWRITE) };
        if base.is_null() {
            // Out of address space: an allocation that cannot be made, as
            // anywhere else in the process.
            std::alloc::handle_alloc_error(
                std::alloc::Layout::from_size_align(len, 4096)
                    .unwrap_or(std::alloc::Layout::new::<u8>()),
            );
        }
        Self {
            base: base.cast(),
            committed: [const { AtomicUsize::new(0) }; SLOTS],
            refused: AtomicBool::new(false),
        }
    }

    pub(super) fn ptr(&self) -> *mut u8 {
        self.base
    }

    /// Make the first `bytes` of the slot `slot`, which begins at `start`,
    /// usable: true once they are, false when the system refuses.
    pub(super) fn commit(&self, slot: usize, start: *mut u8, bytes: usize) -> bool {
        let Some(committed) = self.committed.get(slot) else {
            return false;
        };
        // Relaxed: the producer's own figure, as the field says.
        if committed.load(Ordering::Relaxed) >= bytes {
            return true;
        }
        // SAFETY: `start..start + bytes` lies inside the reservation: the
        // slot's own offset, and a layout the queue has checked against the
        // slot's size. Committing a page that already is changes nothing.
        let done = unsafe { VirtualAlloc(start.cast(), bytes, MEM_COMMIT, PAGE_READWRITE) };
        if done.is_null() {
            if !self.refused.swap(true, Ordering::Relaxed) {
                lowlat_common::log_warn!(
                    "client: the system refused to commit a picture slot, bytes={bytes}"
                );
            }
            return false;
        }
        committed.store(bytes, Ordering::Relaxed);
        true
    }
}

impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: the reservation `new` made, released whole and once.
        unsafe { VirtualFree(self.base.cast(), 0, MEM_RELEASE) };
    }
}

/// A queue's device slots: none here yet.
pub(super) struct DeviceSlots;

impl DeviceSlots {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn backed(&self) -> bool {
        false
    }
}

impl Filling<'_> {
    /// Device planes: none here yet, so none are lent. Creation refuses a
    /// queue of the handle kind on this platform, so nothing asks.
    pub fn device_planes_for(
        &mut self,
        width: u32,
        height: u32,
        format: Format,
    ) -> Option<DevicePlanes> {
        let _ = (width, height, format);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FrameKind, Frames};
    use super::*;

    /// What the system says of the page an address lies in.
    #[repr(C)]
    struct MemoryBasicInformation {
        base: *mut c_void,
        allocation_base: *mut c_void,
        allocation_protect: u32,
        partition: u16,
        region_size: usize,
        state: u32,
        protect: u32,
        kind: u32,
    }

    unsafe extern "system" {
        fn VirtualQuery(
            address: *const c_void,
            info: *mut MemoryBasicInformation,
            length: usize,
        ) -> usize;
    }

    /// Whether the page `address` lies in is committed.
    fn committed(address: *const u8) -> bool {
        let mut info = core::mem::MaybeUninit::<MemoryBasicInformation>::uninit();
        // SAFETY: a query of any address, into a structure of the size said.
        let written = unsafe {
            VirtualQuery(
                address.cast(),
                info.as_mut_ptr(),
                core::mem::size_of::<MemoryBasicInformation>(),
            )
        };
        assert_ne!(written, 0, "the query failed");
        // SAFETY: the system filled it, as `written` says.
        unsafe { info.assume_init() }.state == MEM_COMMIT
    }

    /// **Commit follows the picture, never the reserve.** Nothing is
    /// reserved before the first picture; then the picture's own bytes in
    /// its own slot are committed and no more -- not the rest of that slot,
    /// not the other slots; a larger picture commits further, and what it
    /// wrote stays readable.
    #[test]
    fn a_slot_is_committed_as_far_as_its_picture_reaches() {
        let frames = Frames::new((4096, 4096), FrameKind::Planes);
        assert!(!frames.backed());

        let mut filling = frames.fill().expect("a slot");
        let index = filling.index;
        {
            let planes = filling.planes_for(64, 64, Format::Nv12).expect("planes");
            planes.y[0] = 0x5a;
            let last = planes.uv.len() - 1;
            planes.uv[last] = 0xa5;
        }
        let slot = frames.slot_ptr(index);
        let slot_bytes = frames.slot_bytes();
        // SAFETY: every address below is inside the reservation.
        unsafe {
            assert!(committed(slot), "the picture's first page");
            assert!(
                !committed(slot.add(1 << 20)),
                "a megabyte into the slot, past a 64x64 picture"
            );
            assert!(!committed(slot.add(slot_bytes - 1)), "the slot's last page");
            for other in (0..SLOTS).filter(|s| *s != index) {
                let start = frames.slot_ptr(other);
                assert!(!committed(start), "slot {other}, which holds nothing");
            }
        }

        // A 1080p picture in the same slot reaches further.
        let reach = 1920 * 1080 * 3 / 2;
        {
            let planes = filling
                .planes_for(1920, 1080, Format::Nv12)
                .expect("planes");
            assert_eq!(planes.y[0], 0x5a, "what the first picture wrote");
        }
        // SAFETY: inside the reservation, as above.
        unsafe {
            assert!(committed(slot.add(reach - 1)), "the 1080p picture's end");
            assert!(!committed(slot.add(8 << 20)), "past the 1080p picture");
        }
    }
}
