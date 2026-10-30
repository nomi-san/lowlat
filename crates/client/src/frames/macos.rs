//! The queue's platform half on macOS: host slots in one zeroed allocation,
//! for the codec library's pictures, and the system decoder's own buffers,
//! each held by the slot its picture is published in.
//!
//! **A lent picture is the decoder's buffer**, held from its publish until its
//! slot takes another or the queue goes -- never while the application holds
//! it, since the ring lends a slot to the decoder only once nothing holds it.
//! As planes it is locked for reading, its planes where the decoder wrote
//! them, each at its own pitch; by handle it is held alone and handed out as
//! its surface, which the application's device imports by plane. Nothing is
//! copied either way. Every picture is finished when it is published, so
//! nothing is gated.

use core::ffi::c_void;
use core::time::Duration;
use std::sync::Mutex;

use lowlat_common::latest::{Latest, Taken};
use lowlat_drivers::vt::{Locked, PixelBuffer};

use super::{Filling, Frame, FrameKind, Lent, SLOTS};

/// A picture handed out as its surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Handle {
    /// The picture's `IOSurfaceRef`. **The library's for the lease**: its
    /// slot holds the buffer behind it until the slot takes another picture,
    /// which is after the last hold on it is released.
    pub surface: *const c_void,
    /// The surface's identifier, unique among the surfaces alive. An import
    /// keeps its surface alive, so an identifier an application holds an
    /// import for names nothing else: two frames with the same one share an
    /// import, and a new one is a new import.
    pub allocation: u32,
    /// The surface's whole allocation in bytes.
    pub size: usize,
}

// SAFETY: the surface is the slot's buffer's, which the slot holds for as
// long as the handle may be read; nothing here writes through it.
unsafe impl Send for Handle {}
// SAFETY: as above.
unsafe impl Sync for Handle {}

/// A slot's buffer, held for its picture's lease: locked for reading for a
/// picture of planes, held alone for one handed out as its surface.
#[derive(Debug)]
enum Kept {
    Planes { _locked: Locked },
    Surface { _buffer: PixelBuffer },
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

/// The buffers lent pictures lie in, by slot: written by the producer alone,
/// under a lock only so the drop may happen anywhere.
pub(super) struct DeviceSlots(Mutex<[Option<Kept>; SLOTS]>);

impl DeviceSlots {
    pub(super) fn new() -> Self {
        Self(Mutex::new([const { None }; SLOTS]))
    }

    /// Whether any slot holds a lent buffer.
    pub(super) fn backed(&self) -> bool {
        self.0
            .lock()
            .is_ok_and(|slots| slots.iter().any(Option::is_some))
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
}

impl Filling<'_> {
    /// The decoder's own buffer as this slot's picture, in the kind the
    /// session asks for, decided per picture: by handle its surface, as
    /// planes its planes locked for reading, where they lie either way. The
    /// slot's last buffer goes, which nothing holds now the ring has lent the
    /// slot here. False, the picture lost, where the system would not lock
    /// the buffer, it has no surface or no second plane.
    pub fn lend(&mut self, buffer: PixelBuffer) -> bool {
        if self.frames.kind() == FrameKind::Handle {
            self.lend_surface(buffer)
        } else {
            self.lend_planes(buffer)
        }
    }

    fn lend_planes(&mut self, buffer: PixelBuffer) -> bool {
        let Ok(locked) = buffer.lock() else {
            return false;
        };
        let (Some((y, pitch, _)), Some((uv, uv_pitch, _))) = (locked.plane(0), locked.plane(1))
        else {
            return false;
        };
        if !self.keep(Kept::Planes { _locked: locked }) {
            return false;
        }
        self.pitch = pitch;
        self.uv_pitch = uv_pitch;
        self.uv_offset = 0;
        self.v_offset = 0;
        self.handle = None;
        self.lent = Some(Lent {
            y,
            uv,
            v: core::ptr::null(),
        });
        true
    }

    /// Held, not locked: the application's device reads the surface, each
    /// plane by its index, so the planes' offsets are zero.
    fn lend_surface(&mut self, buffer: PixelBuffer) -> bool {
        let (Some(surface), Some(pitch), Some(uv_pitch)) =
            (buffer.surface(), buffer.pitch(0), buffer.pitch(1))
        else {
            return false;
        };
        if !self.keep(Kept::Surface { _buffer: buffer }) {
            return false;
        }
        self.pitch = pitch;
        self.uv_pitch = uv_pitch;
        self.uv_offset = 0;
        self.v_offset = 0;
        self.lent = None;
        self.handle = Some(Handle {
            surface: surface.surface,
            allocation: surface.id,
            size: surface.size,
        });
        true
    }

    /// The buffer into this slot, the last one going.
    fn keep(&self, kept: Kept) -> bool {
        let Ok(mut slots) = self.frames.device.0.lock() else {
            return false;
        };
        let Some(slot) = slots.get_mut(self.index % SLOTS) else {
            return false;
        };
        *slot = Some(kept);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::{FrameKind, Frames};
    use super::*;
    use lowlat_core::video::{Codec, Rotation, VideoHeader};
    use lowlat_decode::videotoolbox::Backend;
    use lowlat_decode::{Decoder, Format};

    /// A committed clip through the system's decoder, a picture at a time,
    /// each the decoder's own buffer: four-four-four at ten bits, so the
    /// chroma's pitch is its own.
    struct Clip {
        backend: Backend,
        bytes: Vec<u8>,
        at: usize,
    }

    impl Clip {
        fn open() -> Self {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../decode/tests/data/fixtures/hevc-nvenc-444-10.bin"
            );
            let vt = lowlat_drivers::vt::load().unwrap();
            let mut backend = Backend::new(vt, 1 << 22);
            backend
                .build(&VideoHeader {
                    frame_id: 1,
                    width: 0,
                    height: 0,
                    codec: Codec::H265,
                    rotation: Rotation::None,
                    ten_bit: true,
                    locked: false,
                    announced: false,
                    metadata: false,
                })
                .unwrap();
            Self {
                backend,
                bytes: std::fs::read(path).unwrap(),
                at: 0,
            }
        }

        /// The next picture, decoding as many units as it takes.
        fn next(&mut self) -> Option<(lowlat_decode::Picture, PixelBuffer)> {
            loop {
                if let Some(taken) = self.backend.take_buffer().unwrap() {
                    return Some(taken);
                }
                let head = self.bytes.get(self.at..self.at + 4)?;
                let len = u32::from_le_bytes(head.try_into().unwrap()) as usize;
                let unit = &self.bytes[self.at + 4..self.at + 4 + len];
                self.at += 4 + len;
                self.backend.feed(unit).unwrap();
            }
        }
    }

    fn frame(picture: &lowlat_decode::Picture) -> Frame {
        Frame {
            format: picture.format,
            width: picture.width,
            height: picture.height,
            order: picture.order,
            ..Frame::BLANK
        }
    }

    /// **A lent picture is published where it lies**: its planes the
    /// decoder's own, at their own pitches -- the chroma's twice the luma's
    /// samples, interleaved at full size -- with no third plane and no handle,
    /// and the host slots never touched.
    #[test]
    fn a_lent_picture_is_published_where_it_lies() {
        let frames = Frames::new((4096, 4096), FrameKind::Planes);
        let (picture, buffer) = Clip::open().next().expect("a picture");
        assert_eq!(picture.format, Format::P410);
        let mut filling = frames.fill().unwrap();
        assert!(filling.lend(buffer), "the buffer was not lent");
        filling.publish(frame(&picture));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        assert!(!held.y.is_null() && !held.uv.is_null() && held.v.is_null());
        assert!(held.handle.is_none());
        assert!(held.pitch >= 256 * 2 && held.uv_pitch >= 256 * 4);
        assert_eq!(held.frame.uv_pitch, held.uv_pitch);
        assert!(frames.backed(), "the lent buffer is not counted");
        assert!(frames.backing.get().is_none(), "the host slots were made");
        // SAFETY: the slot is held; a sample of each plane is read.
        let _ = unsafe { (*held.y, *held.uv.add(held.uv_pitch - 1)) };
        frames.release(held.index);
    }

    /// **A held slot keeps its buffer** while the decoder goes on and the
    /// other slots take one picture after another: what the application reads
    /// does not change under it, where a buffer let go too soon would be
    /// decoded into again.
    #[test]
    fn a_held_lent_picture_outlives_the_ones_after_it() {
        let frames = Frames::new((4096, 4096), FrameKind::Planes);
        let mut clip = Clip::open();
        let (picture, buffer) = clip.next().expect("a picture");
        let mut filling = frames.fill().unwrap();
        assert!(filling.lend(buffer));
        filling.publish(frame(&picture));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let luma = held.pitch * held.frame.height as usize;
        // SAFETY: the slot is held for the length of the test.
        let first: Vec<u8> = unsafe { core::slice::from_raw_parts(held.y, luma) }.to_vec();
        let mut last = held.seq;
        let mut after = 0;
        while let Some((picture, buffer)) = clip.next() {
            let mut filling = frames.fill().unwrap();
            assert_ne!(filling.index, held.index, "the held slot was lent");
            assert!(filling.lend(buffer));
            filling.publish(frame(&picture));
            if let Ok(Some(next)) = frames.acquire(last, Duration::ZERO) {
                last = next.seq;
                frames.release(next.index);
            }
            after += 1;
        }
        assert_eq!(after, 23, "the clip's pictures after the first");
        // SAFETY: as above.
        let again = unsafe { core::slice::from_raw_parts(held.y, luma) };
        assert!(again == first.as_slice(), "the held picture changed");
        frames.release(held.index);
    }

    /// **A picture by handle is its surface**: the decoder's own, its
    /// identifier and size the surface's, its planes at the surface's
    /// pitches with no offsets, no address handed out; and the kind
    /// switches at the next picture, as the session asks.
    #[test]
    fn a_picture_by_handle_is_its_surface_and_the_kind_switches() {
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        let mut clip = Clip::open();
        let (picture, buffer) = clip.next().expect("a picture");
        let surface = buffer.surface().expect("the buffer's surface");
        let mut filling = frames.fill().unwrap();
        assert!(filling.lend(buffer), "the buffer was not lent");
        filling.publish(frame(&picture));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let handle = held.handle.expect("a handle");
        assert_eq!(
            (handle.surface, handle.allocation, handle.size),
            (surface.surface, surface.id, surface.size)
        );
        assert!(held.y.is_null() && held.uv.is_null() && held.v.is_null());
        assert!(held.pitch >= 256 * 2 && held.uv_pitch >= 256 * 4);
        assert_eq!((held.frame.uv_offset, held.frame.v_offset), (0, 0));
        let last = held.seq;
        frames.release(held.index);

        frames.set_kind(FrameKind::Planes);
        let (picture, buffer) = clip.next().expect("a second picture");
        let mut filling = frames.fill().unwrap();
        assert!(filling.lend(buffer));
        filling.publish(frame(&picture));
        let held = frames.acquire(last, Duration::ZERO).unwrap().unwrap();
        assert!(held.handle.is_none() && !held.y.is_null(), "still a handle");
        frames.release(held.index);
    }

    /// **A held picture by handle keeps its surface** while the decoder goes
    /// on: its slot still holds the buffer behind it once the clip's other
    /// pictures have been lent and published through the other slots.
    #[test]
    fn a_held_picture_by_handle_keeps_its_surface() {
        let frames = Frames::new((4096, 4096), FrameKind::Handle);
        let mut clip = Clip::open();
        let (picture, buffer) = clip.next().expect("a picture");
        let mut filling = frames.fill().unwrap();
        assert!(filling.lend(buffer));
        filling.publish(frame(&picture));
        let held = frames.acquire(0, Duration::ZERO).unwrap().unwrap();
        let handle = held.handle.expect("a handle");
        let mut last = held.seq;
        while let Some((picture, buffer)) = clip.next() {
            let mut filling = frames.fill().unwrap();
            assert_ne!(filling.index, held.index, "the held slot was lent");
            assert!(filling.lend(buffer));
            filling.publish(frame(&picture));
            if let Ok(Some(next)) = frames.acquire(last, Duration::ZERO) {
                last = next.seq;
                frames.release(next.index);
            }
        }
        let kept = frames
            .device
            .0
            .lock()
            .unwrap()
            .get(held.index % SLOTS)
            .and_then(|kept| match kept {
                Some(Kept::Surface { _buffer }) => _buffer.surface().map(|s| s.surface),
                _ => None,
            });
        assert_eq!(kept, Some(handle.surface), "the held surface's buffer went");
        frames.release(held.index);
    }
}
