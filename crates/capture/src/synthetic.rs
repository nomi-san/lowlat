//! The synthetic source.
//!
//! **Not a test double.** It is how everything up to Gate A is built and
//! verified, and it stays afterwards as the reproducible input for
//! performance work, because a real desktop is not reproducible and a
//! regression measured against one cannot be trusted.
//!
//! Its content is a function of the frame index and nothing else, so a
//! decoded picture can be checked against what its source claimed to draw.
//! That is what makes it possible to tell a broken encoder from a broken
//! capture without a human looking at a picture.

use lowlat_common::clock::Time;

use crate::{Frame, Plane};

/// Background, well inside the limited range the wire uses.
const BACKGROUND_LUMA: u8 = 64;
/// The moving bar. Bright enough to find, short of the ceiling.
const BAR_LUMA: u8 = 200;
/// No colour.
const NEUTRAL_CHROMA: u8 = 128;
/// The static block's two chroma components.
///
/// **They differ, and that is the point.** Equal components survive being
/// written in the wrong order, so a consumer that interleaves the two
/// backwards would produce identical output and the fault would keep until
/// something else exposed it.
const BLOCK_CB: u8 = 240;
const BLOCK_CR: u8 = 90;

/// How wide the moving bar is, in luma samples.
const BAR_WIDTH: usize = 64;

/// Mixing constants for the detail band. Odd and large, so neighbouring
/// samples do not correlate and the encoder cannot predict one from another.
const MIX_X: u32 = 2_654_435_761;
const MIX_Y: u32 = 40_503;
const MIX_INDEX: u32 = 2_246_822_519;
/// How far it travels per frame. **Deliberately not a divisor of common
/// widths**, so the pattern does not land on the same columns every few
/// frames and hide a rounding fault in the position.
const BAR_STEP: u64 = 17;

/// Where the bar's left edge is on a given frame.
///
/// The whole content contract in one function: a consumer that knows the
/// index knows what the picture must contain, with no state shared between
/// producer and checker.
pub fn bar_x(index: u64, width: u32) -> u32 {
    if width == 0 {
        return 0;
    }
    u32::try_from(index.wrapping_mul(BAR_STEP) % u64::from(width)).unwrap_or(0)
}

/// A generator of planar frames with known content and controlled motion.
pub struct Synthetic {
    width: usize,
    height: usize,
    luma: Vec<u8>,
    chroma: Vec<u8>,
    chroma_stride: usize,
    index: u64,
    detail_rows: usize,
}

impl core::fmt::Debug for Synthetic {
    /// The planes are megabytes and say nothing useful. Their shape does.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Synthetic")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("index", &self.index)
            .finish()
    }
}

impl Synthetic {
    /// Allocate for one size. **The planes are allocated here and never
    /// again**; producing a frame writes into them.
    pub fn new(width: u32, height: u32) -> Self {
        let width = usize::try_from(width).unwrap_or(0);
        let height = usize::try_from(height).unwrap_or(0);
        // Odd sizes round up rather than being refused: the coded picture is
        // a whole number of macroblocks anyway, and the parameter sets crop
        // back to the real size.
        let chroma_stride = width.div_ceil(2) * 2;
        let chroma_rows = height.div_ceil(2);

        let mut source = Self {
            width,
            height,
            luma: vec![BACKGROUND_LUMA; width * height],
            chroma: vec![NEUTRAL_CHROMA; chroma_stride * chroma_rows],
            chroma_stride,
            index: 0,
            detail_rows: 0,
        };
        source.paint_chroma();
        source
    }

    /// The same source with a band of unpredictable detail across the top.
    ///
    /// **A flat picture tests nothing downstream of the encoder.** A bar on a
    /// flat field codes to a few hundred bytes at any resolution, so every
    /// message fits one fragment and the fragmenting path, the reassembly a
    /// peer runs, and the window arithmetic never meet a message they have to
    /// split. This band is a function of the frame index, so it cannot be
    /// predicted from the frame before it and the encoder has to spend bits.
    ///
    /// **It sits at the top, clear of the row the frame checker samples**, so
    /// the content contract the bar carries is untouched.
    ///
    /// Zero rows is the flat source, and that is the default everywhere: the
    /// latency figures and the refresh sizes on record were measured against
    /// it, and content that changes underneath them would invalidate them
    /// silently.
    pub fn with_detail(width: u32, height: u32, detail_rows: u32) -> Self {
        let mut source = Self::new(width, height);
        source.detail_rows = usize::try_from(detail_rows).unwrap_or(0).min(source.height);
        source
    }

    /// The colour block, written once.
    ///
    /// Chroma never changes, because the motion is carried entirely in luma.
    /// Redrawing it per frame would be two thirds of the work of a frame for
    /// no change in the output.
    fn paint_chroma(&mut self) {
        let rows = self.height.div_ceil(2) / 4;
        let columns = self.width.div_ceil(2) / 4;
        for row in 0..rows {
            let start = row * self.chroma_stride;
            for column in 0..columns {
                let at = start + column * 2;
                if let Some(pair) = self.chroma.get_mut(at..at + 2) {
                    pair[0] = BLOCK_CB;
                    pair[1] = BLOCK_CR;
                }
            }
        }
    }

    /// Rows of unpredictable detail this source paints, from the top.
    pub fn detail_rows(&self) -> usize {
        self.detail_rows
    }

    /// Fill the detail band for one frame.
    ///
    /// A cheap mix of the coordinates and the index. It only has to be
    /// unpredictable to an encoder, not to a cryptographer, and it has to be
    /// a pure function of the index so a checker can still reproduce it.
    fn paint_detail(&mut self, index: u64) {
        let rows = self.detail_rows.min(self.height);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "the low word is the whole of what is wanted from the index"
        )]
        let seed = (index as u32).wrapping_mul(MIX_INDEX);
        for row in 0..rows {
            let start = row * self.width;
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a row index inside a picture"
            )]
            let row_mix = (row as u32).wrapping_mul(MIX_Y) ^ seed;
            let Some(line) = self.luma.get_mut(start..start + self.width) else {
                return;
            };
            for (column, sample) in line.iter_mut().enumerate() {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "a column index inside a picture"
                )]
                let mix = (column as u32).wrapping_mul(MIX_X) ^ row_mix;
                // Kept inside the limited range the wire uses, like the rest
                // of the picture. The mask takes a byte from the middle of the
                // word, where the mixing has spread the input furthest.
                *sample = 16 + u8::try_from((mix >> 13) & 0xFF).unwrap_or(0) % 220;
            }
        }
    }

    /// Produce the next frame.
    ///
    /// Timestamped here, and that stamp is the origin of every latency figure
    /// downstream.
    pub fn acquire(&mut self) -> Frame<'_> {
        let index = self.index;
        self.index = self.index.wrapping_add(1);
        self.draw(index);

        Frame {
            width: u32::try_from(self.width).unwrap_or(0),
            height: u32::try_from(self.height).unwrap_or(0),
            luma: Plane {
                bytes: &self.luma,
                stride: self.width,
            },
            chroma: Plane {
                bytes: &self.chroma,
                stride: self.chroma_stride,
            },
            captured_at: Time::now(),
            index,
        }
    }

    /// How many frames have been produced.
    pub fn produced(&self) -> u64 {
        self.index
    }

    fn draw(&mut self, index: u64) {
        self.luma.fill(BACKGROUND_LUMA);
        self.paint_detail(index);
        let start_column =
            usize::try_from(bar_x(index, u32::try_from(self.width).unwrap_or(0))).unwrap_or(0);
        for row in 0..self.height {
            let row_start = row * self.width;
            for offset in 0..BAR_WIDTH {
                // The bar wraps rather than being clipped, so every frame
                // carries the same amount of moving content and a frame near
                // the edge is not quietly easier to encode than the rest.
                let column = (start_column + offset) % self.width.max(1);
                if let Some(sample) = self.luma.get_mut(row_start + column) {
                    *sample = BAR_LUMA;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_moves_and_wraps_without_leaving_the_picture() {
        let width = 1920;
        assert_eq!(bar_x(0, width), 0);
        assert_eq!(bar_x(1, width), 17);
        assert_eq!(bar_x(2, width), 34);
        // Far enough in to have wrapped many times, and still inside.
        for index in [0u64, 1, 113, 1_000_000, u64::MAX / 3] {
            assert!(bar_x(index, width) < width);
        }
    }

    /// The content contract: what the picture holds is a function of the
    /// index, so a checker needs nothing from the producer but the number.
    #[test]
    fn the_picture_matches_what_the_index_claims() {
        // Wider than the bar, or it wraps onto itself and every column is lit.
        const WIDTH: usize = 256;
        let mut source = Synthetic::new(256, 32);
        for _ in 0..5 {
            let frame = source.acquire();
            let x = usize::try_from(bar_x(frame.index, frame.width)).unwrap_or(0);
            let row = frame.luma.row(3).expect("a row");
            assert_eq!(row[x], BAR_LUMA, "the bar is not where the index says");
            assert_eq!(
                row[(x + BAR_WIDTH - 1) % WIDTH],
                BAR_LUMA,
                "the bar is narrower than it claims"
            );
            assert_eq!(
                row[(x + BAR_WIDTH) % WIDTH],
                BACKGROUND_LUMA,
                "the bar is wider than it claims"
            );
        }
    }

    /// A frame is generated without allocating: the planes were allocated
    /// once and are written in place.
    #[test]
    fn planes_are_allocated_once_and_reused() {
        let mut source = Synthetic::new(64, 32);
        let first = source.acquire().luma.bytes.as_ptr();
        let second = source.acquire().luma.bytes.as_ptr();
        assert_eq!(first, second, "the plane was reallocated between frames");
    }

    /// A swap of the two chroma components would leave this identical if they
    /// were equal, which is why they are not.
    #[test]
    fn the_colour_block_distinguishes_its_two_components() {
        let mut source = Synthetic::new(256, 64);
        let frame = source.acquire();
        let row = frame.chroma.row(0).expect("a chroma row");
        assert_eq!(row[0], BLOCK_CB);
        assert_eq!(row[1], BLOCK_CR);
        assert_ne!(BLOCK_CB, BLOCK_CR, "a swap would be undetectable");

        // Outside the block, in both directions, no colour at all.
        assert_eq!(row[200], NEUTRAL_CHROMA, "the block is too wide");
        let far = frame.chroma.row(20).expect("a chroma row");
        assert_eq!(far[0], NEUTRAL_CHROMA, "the block is too tall");
    }

    #[test]
    fn the_index_counts_from_zero_and_advances() {
        let mut source = Synthetic::new(32, 16);
        assert_eq!(source.produced(), 0);
        assert_eq!(source.acquire().index, 0);
        assert_eq!(source.acquire().index, 1);
        assert_eq!(source.produced(), 2);
    }
}
