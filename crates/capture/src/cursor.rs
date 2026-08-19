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
//!   cycles a pool of them and a redraw lands wherever; only the pixels
//!   distinguish one pointer from another.

use std::os::fd::AsRawFd;

use crate::scanout::{Card, Error, Framebuffer};

/// The most a pointer plane can be, which is what the drivers here advertise.
/// A larger one is refused rather than truncated: a pointer read at the wrong
/// size is worse than no pointer.
const LIMIT: u32 = 256;

/// A pointer, cropped to what is actually drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// Where the drawn part begins inside the plane's own buffer. The
    /// difference matters: the plane is positioned by its buffer's corner, not
    /// by the first opaque pixel.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Reads pointers, reusing one buffer for all of them.
pub struct Reader {
    /// Allocated once. The pointer path is not a frame path, but it is not a
    /// setup path either, and allocating per read would put an allocation on
    /// something that runs whenever the pointer is redrawn.
    scratch: Vec<u8>,
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
            scratch: vec![0; (LIMIT as usize) * (LIMIT as usize) * 4],
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

        let found = extent(pixels, plane.width, plane.height, pitch);
        if let Some(area) = found {
            self.copy(pixels, pitch, area);
        }

        // SAFETY: mapped above, this length, and nothing refers to it after
        // `copy` returned.
        unsafe {
            libc::munmap(mapped, length);
        }

        Ok(found.map(|area| {
            let used = (area.width as usize) * (area.height as usize) * 4;
            (area, self.scratch.get(..used).unwrap_or_default())
        }))
    }

    /// Copy the drawn part out, swapping to the channel order an image wants.
    ///
    /// **The plane's bytes are blue, green, red, alpha**; a picture's are red
    /// first. Copying them across unchanged produces a pointer with red and
    /// blue exchanged, which on a mostly white arrow is invisible and on a
    /// coloured one is not.
    fn copy(&mut self, pixels: &[u8], pitch: usize, area: Extent) {
        let width = area.width as usize;
        for row in 0..area.height as usize {
            let from = (area.y as usize + row) * pitch + (area.x as usize) * 4;
            let to = row * width * 4;
            for column in 0..width {
                let Some(quad) = pixels.get(from + column * 4..from + column * 4 + 4) else {
                    continue;
                };
                let Some(slot) = self.scratch.get_mut(to + column * 4..to + column * 4 + 4) else {
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
        reader.copy(
            &pixels,
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
