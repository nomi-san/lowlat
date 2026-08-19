//! Writing a cursor image in the form the wire carries it.
//!
//! **The pointer travels as a PNG**, so producing one is not optional and not
//! a convenience. It is the only image encoding the far side is prepared to
//! decode on this message, and the checksum of the encoded bytes is what a peer
//! keys its cache on, so two identical pointers must encode to identical bytes.
//!
//! **Stored, not compressed.** The deflate stream is written as uncompressed
//! blocks, which is a legal deflate stream that every decoder reads and which
//! costs no compressor. A pointer is a few thousand bytes at the size these
//! actually are, it is sent only when the shape changes, and a peer that has
//! seen it before is told to reuse it rather than sent it again. Compressing
//! would trade a dependency and a table for bandwidth nothing is short of.

use crate::crc32::Crc32;
use crate::error::{Error, Result};

/// Bytes per pixel in and out: red, green, blue, alpha.
const CHANNELS: usize = 4;

/// The largest run a stored deflate block may carry.
const BLOCK: usize = 0xFFFF;

/// What a picture of this size needs, at most.
///
/// Every byte of the image, a filter marker on each row, the block headers the
/// stored form adds, and the fixed chunks around it. Deliberately an
/// overestimate: a caller sizes a buffer from this once and never has to know
/// how the encoding works.
pub const fn upper_bound(width: u32, height: u32) -> usize {
    let rows = height as usize;
    let raw = rows * (1 + (width as usize) * CHANNELS);
    // Signature, three chunk envelopes, the header body, the zlib wrapper, and
    // five bytes for each stored block the raw data will be cut into.
    8 + 25 + 12 + 12 + 6 + raw + 5 * (raw / BLOCK + 1)
}

/// Write `pixels` as a PNG into `out`, returning how much of it was used.
///
/// `pixels` is red, green, blue, alpha, tightly packed, `width * height` of
/// them. `stride` is the distance between rows in bytes, which is not the width
/// times four when the source is a region of something larger.
pub fn encode(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    out: &mut [u8],
) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(Error::Malformed);
    }
    let mut at = 0;
    put(
        out,
        &mut at,
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
    )?;

    let mut header = [0u8; 13];
    header
        .get_mut(0..4)
        .ok_or(Error::Malformed)?
        .copy_from_slice(&width.to_be_bytes());
    header
        .get_mut(4..8)
        .ok_or(Error::Malformed)?
        .copy_from_slice(&height.to_be_bytes());
    // Eight bits a channel, truecolour with alpha, deflate, no filtering
    // beyond the per-row marker, no interlace.
    *header.get_mut(8).ok_or(Error::Malformed)? = 8;
    *header.get_mut(9).ok_or(Error::Malformed)? = 6;
    chunk(out, &mut at, b"IHDR", &header)?;

    data(pixels, width, height, stride, out, &mut at)?;
    chunk(out, &mut at, b"IEND", &[])?;
    Ok(at)
}

/// The image data chunk, whose body is a zlib stream of the filtered rows.
fn data(
    pixels: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    out: &mut [u8],
    at: &mut usize,
) -> Result<()> {
    let row_bytes = (width as usize) * CHANNELS;
    let raw = (height as usize) * (1 + row_bytes);

    // The chunk's length and checksum are only known once its body is written,
    // so the length is reserved and filled in afterwards.
    let length_at = *at;
    put(out, at, &[0, 0, 0, 0])?;
    let body_at = *at;
    put(out, at, b"IDAT")?;

    // Deflate with no preset dictionary, at the compression level a stored
    // stream reports.
    put(out, at, &[0x78, 0x01])?;

    let mut adler = Adler32::new();
    let mut written = 0usize;
    let mut row = 0usize;
    let mut column = 0usize;
    while written < raw {
        let run = core::cmp::min(BLOCK, raw - written);
        let last = u8::from(written + run == raw);
        put(out, at, &[last])?;
        let length = u16::try_from(run).map_err(|_| Error::Malformed)?;
        put(out, at, &length.to_le_bytes())?;
        put(out, at, &(!length).to_le_bytes())?;

        let mut left = run;
        while left > 0 {
            if column == 0 {
                // The filter marker: none, so the row is its own bytes.
                put(out, at, &[0])?;
                adler.update(&[0]);
                column = 1;
                left -= 1;
                continue;
            }
            let taken = core::cmp::min(left, row_bytes - (column - 1));
            let from = row * stride + (column - 1);
            let slice = pixels.get(from..from + taken).ok_or(Error::Malformed)?;
            put(out, at, slice)?;
            adler.update(slice);
            column += taken;
            left -= taken;
            if column - 1 == row_bytes {
                column = 0;
                row += 1;
            }
        }
        written += run;
    }
    put(out, at, &adler.finish().to_be_bytes())?;

    let body = out.get(body_at..*at).ok_or(Error::Malformed)?;
    let mut crc = Crc32::new();
    crc.update(body);
    let checksum = crc.finish();
    let length = u32::try_from(*at - body_at - 4).map_err(|_| Error::Malformed)?;
    out.get_mut(length_at..length_at + 4)
        .ok_or(Error::Malformed)?
        .copy_from_slice(&length.to_be_bytes());
    put(out, at, &checksum.to_be_bytes())
}

/// One chunk: length, name, body, and the checksum over the name and body.
fn chunk(out: &mut [u8], at: &mut usize, name: &[u8; 4], body: &[u8]) -> Result<()> {
    let length = u32::try_from(body.len()).map_err(|_| Error::Malformed)?;
    put(out, at, &length.to_be_bytes())?;
    put(out, at, name)?;
    put(out, at, body)?;
    let mut crc = Crc32::new();
    crc.update(name);
    crc.update(body);
    put(out, at, &crc.finish().to_be_bytes())
}

fn put(out: &mut [u8], at: &mut usize, bytes: &[u8]) -> Result<()> {
    out.get_mut(*at..*at + bytes.len())
        .ok_or(Error::Malformed)?
        .copy_from_slice(bytes);
    *at += bytes.len();
    Ok(())
}

/// The checksum a deflate stream carries, which is not the one the chunks use.
struct Adler32 {
    low: u32,
    high: u32,
}

impl Adler32 {
    const fn new() -> Self {
        Self { low: 1, high: 0 }
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.low = (self.low + u32::from(*byte)) % 65521;
            self.high = (self.high + self.low) % 65521;
        }
    }

    const fn finish(&self) -> u32 {
        (self.high << 16) | self.low
    }
}

#[cfg(test)]
mod tests {
    use std::vec;
    use std::vec::Vec;

    use super::*;

    /// The published vector, so the deflate checksum is not merely
    /// self-consistent.
    #[test]
    fn the_stream_checksum_matches_its_standard_vector() {
        let mut adler = Adler32::new();
        adler.update(b"Wikipedia");
        assert_eq!(adler.finish(), 0x11E6_0398);
    }

    #[test]
    fn a_picture_larger_than_one_block_is_split() {
        // Wide enough that the rows exceed what one stored block carries, so
        // the splitting path runs rather than being skipped by every size a
        // pointer happens to be.
        let (width, height) = (256u32, 256u32);
        let pixels = vec![0x40u8; (width as usize) * (height as usize) * CHANNELS];
        let mut out = vec![0u8; upper_bound(width, height)];
        let used = encode(
            &pixels,
            width,
            height,
            (width as usize) * CHANNELS,
            &mut out,
        )
        .expect("encode");
        assert!(used > 0xFFFF, "the picture fitted in a single block");
        assert!(used <= out.len(), "the bound was too small");
    }

    #[test]
    fn a_region_of_a_larger_picture_takes_its_stride() {
        // Two rows of a picture four wide, reading only the left half.
        let source: Vec<u8> = (0..2 * 4 * CHANNELS).map(|v| v as u8).collect();
        let mut out = vec![0u8; upper_bound(2, 2)];
        let used = encode(&source, 2, 2, 4 * CHANNELS, &mut out).expect("encode");
        // The second row must come from the second row of the source, not from
        // eight bytes further into the first.
        let expected = source.get(16..24).expect("row");
        assert!(
            out.get(..used)
                .expect("written")
                .windows(8)
                .any(|w| w == expected),
            "the second row was not read at the stride"
        );
    }

    #[test]
    fn nothing_is_written_for_an_empty_picture() {
        let mut out = [0u8; 64];
        assert!(encode(&[], 0, 0, 0, &mut out).is_err());
    }

    #[test]
    fn a_buffer_that_is_too_small_is_refused_rather_than_overrun() {
        let pixels = [0u8; 4 * 4 * CHANNELS];
        let mut out = [0u8; 16];
        assert!(encode(&pixels, 4, 4, 4 * CHANNELS, &mut out).is_err());
    }
}
