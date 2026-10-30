//! The queue's platform half on macOS: host slots in one zeroed allocation,
//! for the codec library's pictures, and the system decoder's own buffers,
//! each held by the slot its picture is published in.
//!
//! **A lent picture is the decoder's buffer, locked for reading** from its
//! publish until its slot takes another or the queue goes -- never while the
//! application holds it, since the ring lends a slot to the decoder only once
//! nothing holds it. Its planes are where the decoder wrote them, each at its
//! own pitch, and nothing is copied. Every picture is finished when it is
//! published, so nothing is gated.

use core::time::Duration;
use std::sync::Mutex;

use lowlat_common::latest::{Latest, Taken};
use lowlat_drivers::vt::{Locked, PixelBuffer};

use super::{Filling, Frame, Lent, SLOTS};

/// No picture leaves as a handle here, so no frame carries one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Handle {}

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
pub(super) struct DeviceSlots(Mutex<[Option<Locked>; SLOTS]>);

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
    /// The decoder's own buffer as this slot's picture, locked for reading:
    /// the planes published are its own, where they lie. The slot's last
    /// buffer goes, which nothing holds now the ring has lent the slot here.
    /// False, the picture lost, where the system would not lock the buffer
    /// or it has no second plane.
    pub fn lend(&mut self, buffer: PixelBuffer) -> bool {
        let Ok(locked) = buffer.lock() else {
            return false;
        };
        let (Some((y, pitch, _)), Some((uv, uv_pitch, _))) = (locked.plane(0), locked.plane(1))
        else {
            return false;
        };
        let Ok(mut slots) = self.frames.device.0.lock() else {
            return false;
        };
        let Some(slot) = slots.get_mut(self.index % SLOTS) else {
            return false;
        };
        *slot = Some(locked);
        self.pitch = pitch;
        self.uv_pitch = uv_pitch;
        self.uv_offset = 0;
        self.v_offset = 0;
        self.lent = Some(Lent {
            y,
            uv,
            v: core::ptr::null(),
        });
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
}
