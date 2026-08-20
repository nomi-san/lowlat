//! Reading the pointer off its plane.
//!
//! **The pointer is the one thing here that is read on the processor**, and
//! deliberately. The rule against moving pixels to system memory exists for
//! frames, which are megabytes at sixty a second; a pointer is a fixed 256x256
//! buffer that changes a few times a second and has to be compared against the
//! last one to know whether it changed at all. There is no comparison that can
//! be done without reading it.
//!
//! Two things measurement decided:
//!
//! - **The allocation is a fixed size whatever the pointer looks like**, and
//!   almost all of it is transparent. What travels is the opaque extent, found
//!   from the alpha channel, and shipping the buffer's own dimensions instead
//!   would send a quarter of a megabyte at the wrong size.
//! - **The identity of the buffer says nothing about the shape.** The display
//!   cycles a pool of them and a redraw lands wherever, including back in the
//!   buffer that already held the previous shape: measured at thirteen of
//!   nineteen shape changes in twenty seconds of ordinary hovering. Only the
//!   pixels distinguish one pointer from another.
//! - **The mapping is uncached, so how it is read matters more than how much
//!   of it is read.** Walking it with a stride costs an order of magnitude
//!   more than copying it out in bulk and walking the copy.

use std::os::fd::AsRawFd;

use crate::scanout::{Card, CursorPlane, Error, Framebuffer};

/// The most a pointer plane can be, which is what the drivers here advertise.
/// A larger one is refused rather than truncated: a pointer read at the wrong
/// size is worse than no pointer.
const LIMIT: u32 = 256;

/// A pointer, cropped to what is actually drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Extent {
    /// Where the drawn part begins inside the plane's own buffer. The
    /// difference matters: the plane is positioned by its buffer's corner, not
    /// by the first opaque pixel.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Rows copied out of the plane before the whole of it is.
///
/// **The plane is a fixed 256 high whatever the pointer is**, and every
/// pointer measured here is drawn within the first few dozen rows of it, so
/// copying the whole allocation spends four times what the answer needs. A
/// pointer whose drawn part reaches the last copied row, or is not in these
/// rows at all, is read again in full: correct either way, and fast for the
/// shapes that actually occur. Measured over a session of ordinary use, the
/// second copy never happened.
const ROWS_FIRST: u32 = 64;

/// Reads between one look at the pixels and the next.
///
/// **The position is nearly free and the picture is not.** Measured on this
/// display: describing the plane costs 0.006 ms, and looking at the pixels
/// costs about 3 ms, most of it faulting a fresh mapping of uncached memory.
/// So the position is read every time and the picture on a cadence, which
/// comes to 0.77 ms per read amortised, and what a guest sees is a pointer
/// that tracks at full rate whose shape can lag by up to four reads.
///
/// **The buffer identifier is not used to trigger this**, tempting as it is:
/// it changes on almost every read while the pointer moves, so it would spend
/// the cost on exactly the reads that can least afford it, and it still misses
/// the change it was supposed to catch.
const PIXELS_EVERY: u32 = 4;

/// Reads pointers, reusing one buffer for all of them.
pub struct Reader {
    /// The plane's own bytes, copied into system memory.
    ///
    /// **The mapping is uncached, and that decides how it is read.** Measured
    /// on this display: touching every fourth byte of the mapping to find the
    /// drawn part costs 43 ms, while copying the same bytes out in bulk costs
    /// 6.6 ms and scanning them here afterwards costs 0.025. Wide loads
    /// combine over that mapping and a strided walk cannot, so everything is
    /// copied first and nothing reads the mapping twice.
    plane: Vec<u8>,
    /// Allocated once. The pointer path is not a frame path, but it is not a
    /// setup path either, and allocating per read would put an allocation on
    /// something that runs whenever the pointer is redrawn.
    scratch: Vec<u8>,
    /// Whether the last read had to copy the whole plane.
    whole: bool,
}

impl core::fmt::Debug for Reader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reader").finish_non_exhaustive()
    }
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    pub fn new() -> Self {
        Self {
            plane: vec![0; (LIMIT as usize) * (LIMIT as usize) * 4],
            scratch: vec![0; (LIMIT as usize) * (LIMIT as usize) * 4],
            whole: false,
        }
    }

    /// Read one pointer, cropped, as red, green, blue, alpha.
    ///
    /// Returns nothing when the pointer is entirely transparent, which is a
    /// real state and not a failure: a compositor draws an empty pointer rather
    /// than removing the plane when an application asks for a blank one.
    pub fn read(
        &mut self,
        card: &Card,
        plane: &Framebuffer,
    ) -> Result<Option<(Extent, &[u8])>, Error> {
        if plane.width > LIMIT || plane.height > LIMIT {
            return Err(Error::UnknownFormat(plane.width));
        }
        let buffer = plane.planes().next().ok_or(Error::NoScanout)?;
        let pitch = buffer.pitch as usize;
        let length = pitch * (plane.height as usize);

        let fd = card.export(buffer)?;
        // SAFETY: the descriptor is a live buffer of at least `length` bytes,
        // mapped read-only and unmapped below before it is closed. Nothing
        // else holds the mapping.
        let mapped = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                length,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            return Err(Error::Device(
                std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
            ));
        }
        // SAFETY: the mapping succeeded and covers `length` bytes. It is not
        // written through, and the slice does not outlive the unmap below.
        let pixels = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), length) };

        // The first rows, then the whole thing only if the answer was not in
        // them. Copying is the whole cost of a read, so the second copy has to
        // be the exception rather than the rule.
        let mut rows = ROWS_FIRST.min(plane.height);
        let mut found = self.take(pixels, pitch, plane.width, rows);
        self.whole = rows >= plane.height;
        if rows < plane.height && !settled(found, rows) {
            rows = plane.height;
            found = self.take(pixels, pitch, plane.width, rows);
            self.whole = true;
        }

        // SAFETY: mapped above, this length, and nothing refers to it after
        // the copies above returned.
        unsafe {
            libc::munmap(mapped, length);
        }

        if let Some(area) = found {
            self.copy(pitch, area);
        }
        Ok(found.map(|area| {
            let used = (area.width as usize) * (area.height as usize) * 4;
            (area, self.scratch.get(..used).unwrap_or_default())
        }))
    }

    /// Whether the last read had to copy the whole plane.
    pub fn read_whole(&self) -> bool {
        self.whole
    }

    /// Copy `rows` of the mapping into system memory and find what is drawn.
    ///
    /// **One bulk copy, then every scan happens here.** The mapping is
    /// uncached and reading it twice, or reading it with a stride, costs an
    /// order of magnitude more than the answer is worth.
    fn take(&mut self, pixels: &[u8], pitch: usize, width: u32, rows: u32) -> Option<Extent> {
        let length = pitch.checked_mul(rows as usize)?;
        let from = pixels.get(..length)?;
        let into = self.plane.get_mut(..length)?;
        into.copy_from_slice(from);
        extent(into, width, rows, pitch)
    }

    /// Copy the drawn part out, swapping to the channel order an image wants.
    ///
    /// **The plane's bytes are blue, green, red, alpha**; a picture's are red
    /// first. Copying them across unchanged produces a pointer with red and
    /// blue exchanged, which on a mostly white arrow is invisible and on a
    /// coloured one is not.
    fn copy(&mut self, pitch: usize, area: Extent) {
        // Named rather than reached through `self`, so reading one buffer and
        // writing the other is two borrows of two fields and not one of both.
        let Self { plane, scratch, .. } = self;
        let width = area.width as usize;
        for row in 0..area.height as usize {
            let from = (area.y as usize + row) * pitch + (area.x as usize) * 4;
            let to = row * width * 4;
            for column in 0..width {
                let Some(quad) = plane.get(from + column * 4..from + column * 4 + 4) else {
                    continue;
                };
                let Some(slot) = scratch.get_mut(to + column * 4..to + column * 4 + 4) else {
                    continue;
                };
                slot[0] = quad[2];
                slot[1] = quad[1];
                slot[2] = quad[0];
                slot[3] = quad[3];
            }
        }
    }
}

/// The pointer as it should reach a peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pointer {
    /// Top-left of the **drawn part** on the display: the plane's own corner
    /// plus where the drawing begins inside it.
    ///
    /// Signed, and it goes negative at the left and top edges. The drawn part
    /// is what travels, so sending the plane's corner instead would place
    /// every pointer up and to the left of itself by however much padding the
    /// compositor happened to leave.
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    /// Names the image, and is what a peer that keeps pointers holds it by.
    pub checksum: u32,
    /// True when this read produced a picture that has not been seen before.
    pub fresh: bool,
    /// True when this read actually examined the pixels.
    ///
    /// **Only a read that looked can say whether a pointer is being drawn.**
    /// The picture is read on a cadence, so most reads report the one they
    /// already had; taking those as evidence that a pointer is still on screen
    /// resets any timer watching for one that has gone, three times out of
    /// four, so it never expires.
    pub looked: bool,
}

/// Reads the pointer and notices when its picture changes.
///
/// **The framebuffer identifier is the trigger and the checksum is the
/// identity.** The display cycles a pool of buffers as the pointer moves and
/// the same picture lands in a different one each time, so an identifier that
/// moved means only that something was redrawn. What settles whether the shape
/// actually changed is the encoded bytes.
pub struct Watcher {
    reader: Reader,
    /// The identifier last looked at, or zero for nothing yet. **Reported,
    /// never used to decide anything.**
    id: u32,
    /// Shapes that arrived in the buffer that carried the previous one, which
    /// is the case an identifier cannot see.
    repeats: u64,
    /// Reads since the pixels were last looked at.
    since: u32,
    /// Pixel reads that had to copy the whole plane after the first rows did
    /// not answer.
    whole: u64,
    /// The current picture, in the form the wire carries it.
    png: Vec<u8>,
    held: Held,
}

/// The picture currently held, and what names it.
#[derive(Debug, Clone, Copy, Default)]
struct Held {
    used: usize,
    checksum: u32,
    extent: Extent,
}

impl core::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Watcher")
            .field("checksum", &self.held.checksum)
            .field("bytes", &self.held.used)
            .field("repeats", &self.repeats)
            .finish_non_exhaustive()
    }
}

impl Default for Watcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Watcher {
    pub fn new() -> Self {
        Self {
            reader: Reader::new(),
            id: 0,
            repeats: 0,
            since: 0,
            whole: 0,
            // Sized for the largest pointer a plane can carry, once, so
            // encoding one never allocates.
            png: vec![0; lowlat_core::png::upper_bound(LIMIT, LIMIT)],
            held: Held::default(),
        }
    }

    /// Read the pointer, if one is being drawn.
    ///
    /// Nothing drawn is a state and not a failure, and it has two causes that
    /// this backend cannot tell apart: an application hid the pointer, or the
    /// compositor drew it into the picture because it outgrew the plane. Both
    /// mean the same thing to a peer, which is not to draw one of its own.
    pub fn read(&mut self, card: &Card, at: &CursorPlane) -> Result<Option<Pointer>, Error> {
        let Some(cursor) = card.cursor_on(at)? else {
            // Forget the identifier: the next plane to appear may reuse it,
            // and its picture would then never be read.
            self.id = 0;
            return Ok(None);
        };

        // **The picture is read on a cadence, and never because the buffer
        // identifier moved.** Triggering on the identifier misses a compositor
        // that redraws a pointer into the buffer it already had: one
        // application's link pointer became an arrow with the identifier
        // unchanged, and a guest kept the hand while the screen showed the
        // arrow. The identifier describes a buffer; the shape is not a
        // property of the buffer it arrived in.
        let repeated = cursor.image.id == self.id;
        self.id = cursor.image.id;
        let mut fresh = false;
        let mut looked = false;
        self.since = self.since.saturating_add(1);
        if self.since >= PIXELS_EVERY || self.held.used == 0 {
            self.since = 0;
            looked = true;
            let Some((extent, rgba)) = self.reader.read(card, &cursor.image)? else {
                return Ok(None);
            };
            // The two fields are named rather than reached through `self`,
            // because the bytes just read are borrowed out of the reader for
            // as long as they are in use.
            fresh = Self::adopt(&mut self.png, &mut self.held, extent, rgba)?;
            // Counted after the picture is adopted, because the bytes it was
            // read into are borrowed out of the reader until then.
            if self.reader.read_whole() {
                self.whole = self.whole.saturating_add(1);
            }
            if fresh && repeated {
                self.repeats = self.repeats.saturating_add(1);
            }
        }

        let held = self.held;
        if held.used == 0 {
            return Ok(None);
        }
        Ok(Some(Pointer {
            x: cursor
                .x
                .saturating_add(i32::try_from(held.extent.x).unwrap_or(0)),
            y: cursor
                .y
                .saturating_add(i32::try_from(held.extent.y).unwrap_or(0)),
            width: u16::try_from(held.extent.width).unwrap_or(u16::MAX),
            height: u16::try_from(held.extent.height).unwrap_or(u16::MAX),
            checksum: held.checksum,
            fresh,
            looked,
        }))
    }

    /// Encode what was read and say whether it is a picture not seen before.
    ///
    /// **Separated from the read so the decision can be tested without a
    /// display**, which is the half that has been got wrong: a redraw is not a
    /// new shape, and treating one as the other sends the same pointer over
    /// and over.
    fn adopt(png: &mut [u8], held: &mut Held, extent: Extent, rgba: &[u8]) -> Result<bool, Error> {
        let stride = (extent.width as usize).saturating_mul(4);
        let used = lowlat_core::png::encode(rgba, extent.width, extent.height, stride, png)
            .map_err(|_| Error::UnknownFormat(extent.width))?;
        let checksum = lowlat_core::crc32::of(png.get(..used).unwrap_or_default());
        if checksum == held.checksum {
            return Ok(false);
        }
        *held = Held {
            used,
            checksum,
            extent,
        };
        Ok(true)
    }

    /// The identifier of the buffer the pointer was last read from.
    pub fn buffer(&self) -> u32 {
        self.id
    }

    /// Pixel reads that fell back to copying the whole plane.
    pub fn whole_reads(&self) -> u64 {
        self.whole
    }

    /// How many shapes arrived in the buffer that carried the one before.
    ///
    /// **The measure of what a buffer identifier cannot tell you.** Anything
    /// above zero is a shape that a reader triggered on the identifier would
    /// have missed entirely.
    pub fn repeated_buffers(&self) -> u64 {
        self.repeats
    }

    /// The picture the last read named, in the form the wire carries it.
    pub fn image(&self) -> &[u8] {
        self.png.get(..self.held.used).unwrap_or_default()
    }
}

/// Whether a partial read answered the question on its own.
///
/// **Nothing found is not an answer**, because the drawn part may be entirely
/// below the rows that were read; nor is a rectangle that reaches the last row
/// read, because it may continue past it.
fn settled(found: Option<Extent>, rows: u32) -> bool {
    found.is_some_and(|area| area.y + area.height < rows)
}

/// The smallest rectangle holding every pixel that is not fully transparent.
fn extent(pixels: &[u8], width: u32, height: u32, pitch: usize) -> Option<Extent> {
    let (mut left, mut top) = (width, height);
    let (mut right, mut bottom) = (0u32, 0u32);
    let mut any = false;
    for y in 0..height {
        let row = (y as usize) * pitch;
        for x in 0..width {
            // Alpha is the fourth byte of each pixel in both orders, which is
            // why this reads it before anything is swapped.
            let Some(alpha) = pixels.get(row + (x as usize) * 4 + 3) else {
                continue;
            };
            if *alpha == 0 {
                continue;
            }
            any = true;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    any.then(|| Extent {
        x: left,
        y: top,
        width: right - left + 1,
        height: bottom - top + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A partial read that did not settle has to ask for the rest.** The
    /// pointer is usually drawn in the first rows of the plane and reading
    /// only those is most of the saving, but a pointer drawn below them, or
    /// one that continues past the last row read, is not answered by them.
    #[test]
    fn a_partial_read_that_did_not_settle_asks_for_the_rest() {
        let at = |y, height| {
            Some(Extent {
                x: 0,
                y,
                width: 8,
                height,
            })
        };
        assert!(!settled(None, 64), "nothing found is not an answer");
        assert!(!settled(at(40, 24), 64), "it reaches the last row read");
        assert!(settled(at(2, 24), 64), "clear of the boundary");
    }

    /// **A redraw is not a new shape.** The display cycles a pool of buffers
    /// and the same pointer lands in a different one as it moves, so the
    /// identifier moves constantly while the picture does not. Reading the
    /// identifier as the identity sends the same image over and over, and on a
    /// peer that keeps them, fills its cache with copies of one pointer.
    #[test]
    fn the_same_picture_read_twice_is_not_a_new_shape() {
        let mut watcher = Watcher::new();
        let area = Extent {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let arrow = [0xFFu8; 2 * 2 * 4];
        let beam = [0x40u8; 2 * 2 * 4];
        let Watcher { png, held, .. } = &mut watcher;

        assert!(
            Watcher::adopt(png, held, area, &arrow).expect("encode"),
            "the first picture"
        );
        let first = held.checksum;
        assert!(
            !Watcher::adopt(png, held, area, &arrow).expect("encode"),
            "the same picture read again"
        );
        assert_eq!(held.checksum, first, "the name of a picture is its bytes");

        assert!(
            Watcher::adopt(png, held, area, &beam).expect("encode"),
            "a real change"
        );
        assert_ne!(held.checksum, first);
        assert!(!watcher.image().is_empty());
    }

    /// Build a plane's worth of bytes with one opaque block in it.
    fn plane(
        width: usize,
        height: usize,
        pitch: usize,
        block: (usize, usize, usize, usize),
    ) -> Vec<u8> {
        let mut pixels = vec![0u8; pitch * height];
        let (bx, by, bw, bh) = block;
        for y in by..by + bh {
            for x in bx..bx + bw {
                let at = y * pitch + x * 4;
                pixels[at] = 0x11; // blue
                pixels[at + 1] = 0x22; // green
                pixels[at + 2] = 0x33; // red
                pixels[at + 3] = 0xFF; // opaque
            }
        }
        let _ = width;
        pixels
    }

    /// **The whole point of cropping.** A pointer occupies a small part of a
    /// fixed allocation, and the extent has to be the drawn part rather than
    /// the buffer, or a few hundred bytes becomes a quarter of a megabyte at
    /// the wrong size.
    #[test]
    fn the_extent_is_the_drawn_part_and_not_the_buffer() {
        let pitch = 256 * 4;
        let pixels = plane(256, 256, pitch, (7, 2, 21, 24));
        let area = extent(&pixels, 256, 256, pitch).expect("something is drawn");
        assert_eq!(
            area,
            Extent {
                x: 7,
                y: 2,
                width: 21,
                height: 24
            }
        );
    }

    /// A pointer drawn as nothing is a state, not a fault.
    #[test]
    fn a_transparent_plane_yields_no_extent() {
        let pitch = 64 * 4;
        assert!(extent(&vec![0u8; pitch * 64], 64, 64, pitch).is_none());
    }

    /// A single opaque pixel is one pixel wide, not zero. The bounds are
    /// inclusive and the arithmetic that converts them is where that is easy
    /// to get wrong.
    #[test]
    fn one_opaque_pixel_is_one_pixel_wide() {
        let pitch = 16 * 4;
        let pixels = plane(16, 16, pitch, (5, 9, 1, 1));
        let area = extent(&pixels, 16, 16, pitch).expect("something is drawn");
        assert_eq!(
            area,
            Extent {
                x: 5,
                y: 9,
                width: 1,
                height: 1
            }
        );
    }

    /// The row stride is the driver's, not the width, and reading by width
    /// walks progressively further into the wrong row.
    #[test]
    fn the_extent_reads_rows_at_the_stride() {
        // A pitch wider than the picture, with the block placed so that reading
        // at width instead of pitch would find it in the wrong place.
        let pitch = 128 * 4;
        let pixels = plane(32, 32, pitch, (3, 4, 2, 2));
        let area = extent(&pixels, 32, 32, pitch).expect("something is drawn");
        assert_eq!(area.x, 3);
        assert_eq!(area.y, 4);
    }

    /// Blue and red exchange places on the way out, and a pointer that is
    /// mostly white hides the mistake.
    #[test]
    fn the_channel_order_is_swapped_on_the_way_out() {
        let pitch = 8 * 4;
        let pixels = plane(8, 8, pitch, (1, 1, 2, 2));
        let mut reader = Reader::new();
        reader.plane[..pixels.len()].copy_from_slice(&pixels);
        reader.copy(
            pitch,
            Extent {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            },
        );
        // Written as blue 0x11, green 0x22, red 0x33; read back red first.
        assert_eq!(
            reader.scratch.get(..4).expect("a pixel"),
            &[0x33, 0x22, 0x11, 0xFF]
        );
    }
}
