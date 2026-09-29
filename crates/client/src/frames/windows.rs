//! The queue's platform half on Windows: host slots committed as far as the
//! pictures laid out in them reach, and device slots of shared textures, one
//! per plane, on the decoder's own device.
//!
//! **A device slot carries its own backing**: the textures, the GPU they are
//! on and the backing generation they were made in. The session's backing
//! moves on when the decoder opens on another device -- a GPU chosen, the
//! device lost and found again -- and a slot is made again the next time the
//! ring lends it to the producer, never while the application holds it, so a
//! held picture stays valid on its old GPU until it is released.
//!
//! **A picture of textures is published before it is finished** and handed
//! out only once the fence of the device it was made on has passed it: its
//! gate carries the backing generation in its high bits and the fence's
//! value below them. A gate of an older generation never opens -- its device
//! is not the one whose fence is read, and a picture queued there when the
//! session moved on is not waited for -- nor does one of a device that is
//! gone, whose fence reads past every value.
//!
//! **A backing the vendor's runtime writes registers each slot's textures
//! with it** when the slot is made, and the slot keeps the registrations,
//! and the runtime they need, until it is made again.

use core::cell::{RefCell, UnsafeCell};
use core::ffi::c_void;
use core::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lowlat_common::latest::{Gate, Latest, Taken};
use lowlat_decode::Format;
use lowlat_decode::d3d11::plane_textures;
use lowlat_drivers::cuda::{Context, Cuda, Registered};
use lowlat_drivers::d3d11::{Device, Event, Fence, Luid, SharedTexture};

use super::{Filling, Frame, Frames, SLOTS};

/// The textures behind a device slot, as the application is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle {
    /// Each plane's shared handle, zero for a plane the layout does not
    /// have. **The library's for the lease**: valid until the slot is made
    /// again, which is after the last hold on it is released, and reused by
    /// the system once its texture is freed.
    pub textures: [u64; 3],
    /// The GPU the textures are on.
    pub adapter: Luid,
    /// The slot's backing's ordinal since creation, from one. A handle is
    /// reused once its texture is freed, so this is what tells one backing
    /// from the next: two frames with the same ordinal share an open, and a
    /// new ordinal is a new open.
    pub allocation: u32,
}

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

/// Bits of a gate below the backing generation: the fence's value.
const VALUE_BITS: u32 = 48;
const VALUE_MASK: u64 = (1 << VALUE_BITS) - 1;
/// The bits of a generation a gate keeps, above the value: the generation
/// wraps there, and is compared in that wrapping order.
const TAG_MASK: u64 = u64::MAX >> VALUE_BITS;

fn tag(generation: u32) -> u64 {
    u64::from(generation) & TAG_MASK
}

/// Whether `gate` was published on the backing of `generation`.
fn names(gate: u64, generation: u32) -> bool {
    gate >> VALUE_BITS == tag(generation)
}

/// Whether `gate` was published on a backing newer than `generation`'s, in
/// the wrapping order of the bits a gate keeps.
fn newer(gate: u64, generation: u32) -> bool {
    let ahead = (gate >> VALUE_BITS).wrapping_sub(tag(generation)) & TAG_MASK;
    ahead != 0 && ahead <= TAG_MASK / 2
}

/// The vendor's runtime a backing's textures are written through, in the
/// context they are registered in. The context first, so it is let go
/// while the runtime it came from is still loaded.
#[derive(Clone, Debug)]
pub struct Vendor {
    pub context: Arc<Context>,
    pub cuda: Arc<Cuda>,
}

/// One device slot: its textures and what they were made for.
struct DeviceSlot {
    /// Each texture's registration with the vendor's runtime, on a backing
    /// it writes: first, so each is let go before its texture is.
    registered: [Option<Registered>; 3],
    planes: [Option<SharedTexture>; 3],
    layout: (Format, u32, u32),
    generation: u32,
    adapter: Luid,
    allocation: u32,
    /// What the registrations need to the last: after them.
    vendor: Option<Vendor>,
}

/// The session's backing now: the device slots are made on it, and its fence
/// says when a picture made on it is finished.
#[derive(Clone)]
struct Current {
    generation: u32,
    device: Arc<Device>,
    fence: Arc<Fence>,
    /// The vendor's runtime, on a backing it writes.
    vendor: Option<Vendor>,
}

/// A queue's device slots.
pub(super) struct DeviceSlots {
    /// Each slot's backing. **Touched only by the producer the ring has lent
    /// the slot to**, and by the drop.
    slots: [UnsafeCell<Option<DeviceSlot>>; SLOTS],
    /// Taken by the producer to make a slot's textures or to move the
    /// backing on, and by an acquire to read the fence -- never held across
    /// a wait.
    current: Mutex<Option<Current>>,
    /// The current backing's generation, for the producer's gate and slot
    /// checks without the lock; zero before the first backing.
    generation: AtomicU32,
    /// Ordinals handed out so far.
    allocations: AtomicU32,
    /// What an acquire waiting on a fence sleeps on; a close sets it.
    event: Option<Event>,
}

// SAFETY: a slot's cell is touched only by the producer holding that slot
// FILLING, which the ring grants one thread at a time with the transitions
// ordering the accesses, and by the drop, which has the queue alone; the
// textures and devices inside are free-threaded. The rest is atomics, a
// lock and an event.
unsafe impl Send for DeviceSlots {}
unsafe impl Sync for DeviceSlots {}

impl DeviceSlots {
    pub(super) fn new() -> Self {
        Self {
            slots: [const { UnsafeCell::new(None) }; SLOTS],
            current: Mutex::new(None),
            generation: AtomicU32::new(0),
            allocations: AtomicU32::new(0),
            event: Event::new().ok(),
        }
    }

    /// Whether any device slot has been made yet.
    pub(super) fn backed(&self) -> bool {
        self.allocations.load(Ordering::Relaxed) > 0
    }

    fn current(&self) -> Option<Current> {
        self.current.lock().ok().and_then(|c| c.clone())
    }

    /// The gate a picture finished at the current backing's `value` is
    /// published behind; zero for one finished now.
    pub(super) fn gate(&self, value: u64) -> u64 {
        if value == 0 {
            return 0;
        }
        (tag(self.generation.load(Ordering::Relaxed)) << VALUE_BITS) | (value & VALUE_MASK)
    }

    /// The newest picture whose gate is open, waiting up to `timeout`: for
    /// a publish, or for the fence of the picture next to finish.
    pub(super) fn acquire(
        &self,
        ring: &Latest<Frame, SLOTS>,
        after: u64,
        timeout: Duration,
    ) -> Option<Taken<Frame>> {
        // The backing is read once here and again only when a gate names a
        // newer one, which is when the session has moved on meanwhile.
        let current = RefCell::new(None::<Current>);
        let refresh = |gate: u64| {
            let mut held = current.borrow_mut();
            if held.as_ref().is_none_or(|c| newer(gate, c.generation)) {
                *held = self.current();
            }
        };
        let check = |gate: u64| {
            if gate == 0 {
                return Gate::Open;
            }
            refresh(gate);
            let held = current.borrow();
            match held.as_ref() {
                Some(c) if names(gate, c.generation) => match c.fence.completed() {
                    // A fence past every value is a device that is gone.
                    u64::MAX => Gate::Never,
                    done if done >= gate & VALUE_MASK => Gate::Open,
                    _ => Gate::Shut,
                },
                _ => Gate::Never,
            }
        };
        let wait = |gate: u64, left: Duration| {
            let Some(event) = self.event.as_ref() else {
                return;
            };
            let held = current.borrow();
            let asked = held
                .as_ref()
                .filter(|c| names(gate, c.generation))
                .is_some_and(|c| c.fence.notify_at(gate & VALUE_MASK, event).is_ok());
            // A fence that cannot be asked is read again after a moment, so
            // the wait never turns into a poll.
            event.wait(if asked {
                left
            } else {
                left.min(Duration::from_millis(1))
            });
        };
        ring.acquire_gated(after, timeout, check, wait)
    }

    /// Wake an acquire sleeping on a fence: the queue is closing.
    pub(super) fn wake(&self) {
        if let Some(event) = self.event.as_ref() {
            event.set();
        }
    }
}

impl Frames {
    /// The session's backing moves to `device`, whose `fence` says when a
    /// picture made on it is finished, and whose textures `vendor` writes
    /// where it is given. Called on the decode thread each time it opens a
    /// device that can hand pictures out as textures; a slot of an older
    /// backing is made again when it is next lent, and a picture of one not
    /// yet taken is never handed out.
    pub fn open_device(&self, device: Arc<Device>, fence: Arc<Fence>, vendor: Option<Vendor>) {
        let generation = self.device.generation.load(Ordering::Relaxed) + 1;
        if let Ok(mut current) = self.device.current.lock() {
            *current = Some(Current {
                generation,
                device,
                fence,
                vendor,
            });
        }
        self.device.generation.store(generation, Ordering::Relaxed);
    }
}

impl Filling<'_> {
    /// The textures to split a `width` x `height` picture of `format` into,
    /// one per plane, on the current backing: the slot's own if they were
    /// made for this layout on this backing, else fresh ones in their place.
    /// `None` without a backing, or when the device refuses.
    pub fn textures_for(
        &mut self,
        width: u32,
        height: u32,
        format: Format,
    ) -> Option<[Option<&SharedTexture>; 3]> {
        let s = self.device_slot(width, height, format)?;
        Some(s.planes.each_ref().map(Option::as_ref))
    }

    /// As [`Self::textures_for`], each texture as the vendor's runtime
    /// writes it: `None` also on a backing it does not write, or when it
    /// refused a texture.
    pub fn registered_for(
        &mut self,
        width: u32,
        height: u32,
        format: Format,
    ) -> Option<[Option<&Registered>; 3]> {
        let s = self.device_slot(width, height, format)?;
        s.vendor.as_ref()?;
        Some(s.registered.each_ref().map(Option::as_ref))
    }

    /// The slot lent, made for a `width` x `height` picture of `format` on
    /// the current backing if it was not already, and the handle the
    /// picture will carry.
    fn device_slot(&mut self, width: u32, height: u32, format: Format) -> Option<&DeviceSlot> {
        let slots = &self.frames.device;
        let generation = slots.generation.load(Ordering::Relaxed);
        let cell = slots.slots.get(self.index)?;
        // SAFETY: the ring has lent this slot to this producer, and it stays
        // lent while `self` lives; no other thread touches the cell meanwhile.
        let slot = unsafe { &mut *cell.get() };
        let layout = (format, width, height);
        if slot
            .as_ref()
            .is_none_or(|s| s.layout != layout || s.generation != generation)
        {
            // The textures before these drop here, which is safe because the
            // ring lent this slot to the producer, so nothing holds it.
            *slot = None;
            let current = slots.current()?;
            let mut planes = [None, None, None];
            for (plane, texture) in planes.iter_mut().zip(plane_textures(format, width, height)) {
                if let Some((format, w, h)) = texture {
                    *plane = Some(current.device.shared_texture(format, w, h).ok()?);
                }
            }
            let mut registered = [None, None, None];
            if let Some(vendor) = &current.vendor {
                for (registration, plane) in registered.iter_mut().zip(&planes) {
                    if let Some(texture) = plane {
                        // SAFETY: a live texture of the backing's device, on
                        // the adapter the context's device is; the slot keeps
                        // the texture, the context and the runtime past the
                        // registration, which drops first.
                        let made = unsafe {
                            vendor
                                .cuda
                                .register_texture(&vendor.context, texture.texture().cast())
                        };
                        *registration = Some(made.ok()?);
                    }
                }
            }
            let allocation = slots.allocations.fetch_add(1, Ordering::Relaxed) + 1;
            *slot = Some(DeviceSlot {
                registered,
                planes,
                layout,
                generation: current.generation,
                adapter: current.device.adapter.luid,
                allocation,
                vendor: current.vendor,
            });
        }
        let s = slot.as_ref()?;
        self.pitch = 0;
        self.uv_offset = 0;
        self.v_offset = 0;
        self.handle = Some(Handle {
            textures: s
                .planes
                .each_ref()
                .map(|p| p.as_ref().map_or(0, |t| t.handle)),
            adapter: s.adapter,
            allocation: s.allocation,
        });
        Some(s)
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

    /// A gate carries the backing generation above the fence's value, and a
    /// picture finished now is published open.
    #[test]
    fn a_gate_is_the_generation_above_the_value() {
        let frames = Frames::new((64, 64), FrameKind::Handle);
        assert_eq!(frames.device.gate(0), 0);
        frames.device.generation.store(3, Ordering::Relaxed);
        assert_eq!(frames.device.gate(7), (3 << VALUE_BITS) | 7);
        assert!(names(frames.device.gate(7), 3));
        assert!(!names(frames.device.gate(7), 2), "another backing's gate");
    }

    /// **A gate names its backing past the bits it keeps of it**: a session
    /// that has opened more devices than the gate has room to count still
    /// hands its pictures out, and an older backing's gate is still refused.
    #[test]
    fn a_gate_names_its_backing_past_the_bits_it_keeps() {
        let frames = Frames::new((64, 64), FrameKind::Handle);
        let many = (1u32 << (64 - VALUE_BITS)) + 3;
        frames.device.generation.store(many, Ordering::Relaxed);
        let gate = frames.device.gate(7);
        assert!(names(gate, many), "the backing's own picture refused");
        assert!(!names(gate, many - 1), "an older backing's picture taken");
        // Newer across the wrap, so an acquire holding the backing before it
        // reads the session's again.
        assert!(newer(gate, many - 4), "a newer backing read as older");
        assert!(!newer(gate, many), "a backing newer than itself");
        frames.device.generation.store(many - 4, Ordering::Relaxed);
        assert!(
            !newer(frames.device.gate(7), many),
            "an older backing read as newer"
        );
    }

    fn frame(order: i32) -> Frame {
        Frame {
            order,
            width: 64,
            height: 64,
            ..Frame::BLANK
        }
    }

    /// A queue asking for handles with a backing on the first GPU offered,
    /// and that backing's device and fence; `None` without a GPU.
    fn texture_queue() -> Option<(Frames, Arc<Device>, Arc<Fence>)> {
        let d3d11 = lowlat_drivers::d3d11::D3d11::load().ok()?;
        let luid = d3d11
            .adapters()
            .ok()?
            .into_iter()
            .find(|a| a.decodes_here())?
            .luid;
        let device = Arc::new(d3d11.open(luid).ok()?);
        let fence = Arc::new(device.fence(0).ok()?);
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        frames.open_device(Arc::clone(&device), Arc::clone(&fence), None);
        Some((frames, device, fence))
    }

    /// A picture of textures published behind `value`.
    fn publish(frames: &Frames, order: i32, value: u64) -> Handle {
        let mut filling = frames.fill().expect("a slot");
        let planes = filling
            .textures_for(1280, 720, Format::Nv12)
            .expect("textures");
        assert!(planes[0].is_some() && planes[1].is_some() && planes[2].is_none());
        let handle = filling.handle.expect("the slot's handle");
        filling.publish_gated(frame(order), value);
        handle
    }

    /// **A slot's textures are made once per layout, and the picture says
    /// where they are**: a handle per plane the layout has, the GPU they are
    /// on, and the same backing each time the slot is lent for the same
    /// layout.
    #[test]
    #[ignore = "requires a GPU"]
    fn a_texture_slot_is_made_once_per_layout() {
        let Some((frames, device, fence)) = texture_queue() else {
            println!("no GPU; not exercised");
            return;
        };
        let mut by_slot = [None; SLOTS];
        let mut last = 0;
        for n in 1..20u64 {
            let handle = publish(&frames, n as i32, n);
            device.signal(&fence, n).expect("a signal");
            let held = frames
                .acquire(last, Duration::from_secs(2))
                .unwrap()
                .expect("the picture, once finished");
            last = held.seq;
            assert_eq!(held.handle, Some(handle));
            assert_eq!(handle.adapter, device.adapter.luid);
            assert!(handle.textures[0] != 0 && handle.textures[1] != 0);
            assert_eq!(handle.textures[2], 0, "a plane the layout does not have");
            let seen = by_slot[held.index].get_or_insert(handle);
            assert_eq!(*seen, handle, "slot {} made again", held.index);
            frames.release(held.index);
        }
        assert!(
            by_slot
                .iter()
                .flatten()
                .all(|h| h.allocation <= SLOTS as u32)
        );
    }

    /// **A picture is handed out only once its device work is finished**,
    /// and one of a backing the session has moved on from never is.
    #[test]
    #[ignore = "requires a GPU"]
    fn an_unfinished_picture_waits_and_an_old_backings_never_comes() {
        let Some((frames, device, fence)) = texture_queue() else {
            println!("no GPU; not exercised");
            return;
        };
        publish(&frames, 1, 5);
        assert!(
            frames
                .acquire(0, Duration::from_millis(30))
                .unwrap()
                .is_none(),
            "a picture was handed out before its fence passed"
        );
        device.signal(&fence, 5).expect("a signal");
        let held = frames
            .acquire(0, Duration::from_secs(2))
            .unwrap()
            .expect("the picture, once finished");
        frames.release(held.index);

        // Published on the old backing, then the session moves on: even
        // with both fences past its value, it never comes out -- the new
        // one's reaching it says nothing of the old device's work.
        publish(&frames, 2, 6);
        let moved = Arc::new(device.fence(0).expect("a fence"));
        frames.open_device(Arc::clone(&device), Arc::clone(&moved), None);
        device.signal(&fence, 6).expect("a signal");
        device.signal(&moved, 10).expect("a signal");
        assert!(
            frames
                .acquire(held.seq, Duration::from_millis(30))
                .unwrap()
                .is_none(),
            "a picture of an older backing was handed out"
        );
        // The new backing's pictures come out as ever.
        publish(&frames, 3, 11);
        device.signal(&moved, 11).expect("a signal");
        let fresh = frames
            .acquire(held.seq, Duration::from_secs(2))
            .unwrap()
            .expect("the new backing's picture");
        assert_eq!(fresh.frame.order, 3);
    }

    /// **A held picture outlives a move to another backing**: every other
    /// slot is made again on the new one, and the held slot's textures still
    /// open until it is released and lent again.
    #[test]
    #[ignore = "requires a GPU"]
    fn a_held_texture_slot_outlives_a_move() {
        let Some((frames, device, fence)) = texture_queue() else {
            println!("no GPU; not exercised");
            return;
        };
        let old = publish(&frames, 1, 1);
        device.signal(&fence, 1).expect("a signal");
        let held = frames.acquire(0, Duration::from_secs(2)).unwrap().unwrap();

        let moved = Arc::new(device.fence(0).expect("a fence"));
        frames.open_device(Arc::clone(&device), Arc::clone(&moved), None);
        let mut last = held.seq;
        for n in 1..5u64 {
            let handle = publish(&frames, n as i32 + 1, n);
            assert!(
                handle.allocation > old.allocation,
                "a slot kept its old backing"
            );
            device.signal(&moved, n).expect("a signal");
            let h = frames
                .acquire(last, Duration::from_secs(2))
                .unwrap()
                .unwrap();
            assert_ne!(h.index, held.index, "the held slot was lent");
            last = h.seq;
            frames.release(h.index);
        }
        let other = lowlat_drivers::d3d11::D3d11::load()
            .unwrap()
            .open(device.adapter.luid)
            .unwrap();
        assert!(
            other.open_shared(old.textures[0]).is_ok(),
            "the held picture's texture was freed under it"
        );
        frames.release(held.index);
    }

    /// A queue asking for handles with a backing the vendor's runtime
    /// writes, on its first GPU, with that backing's device, fence and
    /// runtime; `None` without one.
    fn vendor_queue() -> Option<(Frames, Arc<Device>, Arc<Fence>)> {
        let cuda = Cuda::load().ok()?;
        let compute = cuda.any_device().ok()?;
        let value = cuda.luid(&compute).ok()?;
        let context = cuda.retain_primary(&compute).ok()?;
        let d3d11 = lowlat_drivers::d3d11::D3d11::load().ok()?;
        let luid = d3d11
            .adapters()
            .ok()?
            .into_iter()
            .map(|a| a.luid)
            .find(|l| l.value() == value)?;
        let device = Arc::new(d3d11.open(luid).ok()?);
        let fence = Arc::new(device.fence(0).ok()?);
        let vendor = Vendor {
            context: Arc::new(context),
            cuda: Arc::new(cuda),
        };
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        frames.open_device(Arc::clone(&device), Arc::clone(&fence), Some(vendor));
        Some((frames, device, fence))
    }

    /// **A backing the vendor's runtime writes registers its slots' textures
    /// with it, and one it does not write lends none**: after a move to the
    /// system's decoder on the same GPU, a slot is made again unregistered,
    /// and the held slot's textures still open until it is released.
    #[test]
    #[ignore = "requires the vendor's GPU"]
    fn a_vendor_backing_registers_its_slots_and_a_held_one_outlives_a_move() {
        let Some((frames, device, fence)) = vendor_queue() else {
            println!("no vendor's GPU; not exercised");
            return;
        };
        let mut filling = frames.fill().expect("a slot");
        let registered = filling
            .registered_for(1280, 720, Format::Nv12)
            .expect("the slot's textures, registered");
        assert!(registered[0].is_some() && registered[1].is_some() && registered[2].is_none());
        let old = filling.handle.expect("the slot's handle");
        filling.publish_gated(frame(1), 1);
        device.signal(&fence, 1).expect("a signal");
        let held = frames.acquire(0, Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(held.handle, Some(old));

        let moved = Arc::new(device.fence(0).expect("a fence"));
        frames.open_device(Arc::clone(&device), Arc::clone(&moved), None);
        let mut filling = frames.fill().expect("a slot");
        assert_ne!(filling.index, held.index, "the held slot was lent");
        assert!(
            filling.registered_for(1280, 720, Format::Nv12).is_none(),
            "a backing the runtime does not write lent registrations"
        );
        let handle = filling.handle.expect("the slot's handle");
        assert!(
            handle.allocation > old.allocation,
            "a slot kept its old backing"
        );
        drop(filling);
        let other = lowlat_drivers::d3d11::D3d11::load()
            .unwrap()
            .open(device.adapter.luid)
            .unwrap();
        assert!(
            other.open_shared(old.textures[0]).is_ok(),
            "the held picture's texture was freed under it"
        );
        frames.release(held.index);
    }

    /// **A close wakes an acquire sleeping on a fence**, which would
    /// otherwise wait its whole timeout for work that never finishes.
    #[test]
    #[ignore = "requires a GPU"]
    fn a_close_wakes_a_wait_on_a_fence() {
        let Some((frames, _device, _fence)) = texture_queue() else {
            println!("no GPU; not exercised");
            return;
        };
        let frames = Arc::new(frames);
        publish(&frames, 1, 9);
        let waiter = {
            let frames = Arc::clone(&frames);
            std::thread::spawn(move || {
                let began = std::time::Instant::now();
                let got = frames.acquire(0, Duration::from_secs(10)).unwrap();
                (got.is_none(), began.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        frames.close();
        let (empty, waited) = waiter.join().unwrap();
        assert!(empty, "a close handed out an unfinished picture");
        assert!(
            waited < Duration::from_secs(2),
            "the close did not wake it: {waited:?}"
        );
    }
}
