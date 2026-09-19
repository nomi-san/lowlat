//! The host's pointer: the pictures it sends, the ones it names, and the
//! reader that turns one into pixels.
//!
//! The initialization declares that this client caches, so a host sends a
//! picture once and names it afterwards by the checksum of its bytes
//! (docs/10-client.md section 7). The cache here is what makes that name
//! resolve; it empties when the host's forget bit says the host's own has.
//!
//! The picture travels as a PNG and leaves the library as RGBA: the reader
//! takes the container apart, inflates the one stream inside it and undoes
//! the row filters. It reads what a pointer needs -- 8-bit RGB or RGBA, one
//! pass, up to 512 square -- and refuses the rest, so the fuzz surface is the
//! width of the format actually used and not the whole standard.

use lowlat_core::crc32;

/// Pictures the cache holds, which is the number a host sends before it
/// tells the far side to forget them all.
pub const CAPACITY: usize = 100;

/// The widest and tallest picture the reader takes.
pub const MAX_SIDE: u32 = 512;

/// Decoded bytes at the ceiling: 512 square, four bytes a pixel, 1 MiB.
pub const MAX_DECODED: usize = (MAX_SIDE as usize) * (MAX_SIDE as usize) * 4;

/// The longest PNG the cache keeps or the reader looks at. A picture at the
/// ceiling stored uncompressed is the decoded size plus a few bytes per
/// block; anything past that cannot be a picture the reader would accept.
pub const MAX_PNG: usize = MAX_DECODED + 64 * 1024;

/// What the cache holds in all, past which it starts over.
const MAX_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// One picture a host may name again.
#[derive(Debug, Clone)]
struct Entry {
    checksum: u32,
    width: u16,
    height: u16,
    hot_x: u16,
    hot_y: u16,
    png: Vec<u8>,
}

/// The size and hotspot a name resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
}

/// Pictures by checksum, as the host expects this client to keep them.
#[derive(Debug, Default)]
pub struct Cache {
    entries: Vec<Entry>,
    bytes: usize,
}

impl Cache {
    /// Keep a picture the host just sent, keyed by the checksum it will name
    /// it by. Returns that checksum.
    ///
    /// **Full means start over, not evict one.** The host counts what it
    /// sent and tells this side to forget at its own capacity; a cache that
    /// evicted one at a time would drop a picture the host still names.
    pub fn insert(&mut self, shape: Shape, png: &[u8]) -> u32 {
        let checksum = crc32::of(png);
        if let Some(entry) = self.entries.iter_mut().find(|e| e.checksum == checksum) {
            entry.width = shape.width;
            entry.height = shape.height;
            entry.hot_x = shape.hot_x;
            entry.hot_y = shape.hot_y;
            return checksum;
        }
        if self.entries.len() >= CAPACITY || self.bytes.saturating_add(png.len()) > MAX_CACHE_BYTES
        {
            lowlat_common::log_warn!(
                "client: cursor cache full, forgetting all, entries={} bytes={}",
                self.entries.len(),
                self.bytes
            );
            self.clear();
        }
        self.bytes = self.bytes.saturating_add(png.len());
        self.entries.push(Entry {
            checksum,
            width: shape.width,
            height: shape.height,
            hot_x: shape.hot_x,
            hot_y: shape.hot_y,
            png: png.to_vec(),
        });
        checksum
    }

    /// The picture a name resolves to, with the shape stored beside it.
    pub fn get(&self, checksum: u32) -> Option<(Shape, &[u8])> {
        let entry = self.entries.iter().find(|e| e.checksum == checksum)?;
        Some((
            Shape {
                width: entry.width,
                height: entry.height,
                hot_x: entry.hot_x,
                hot_y: entry.hot_y,
            },
            &entry.png,
        ))
    }

    /// Forget every picture: the host's forget bit, or a full cache.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Why a picture was not decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a PNG at all.
    NotPng,
    /// A PNG the reader could not follow: a chunk past the end, no header, a
    /// stream shorter or longer than the picture.
    Malformed,
    /// A PNG of a kind the reader does not take: not 8-bit RGB or RGBA, or
    /// interlaced.
    Unsupported,
    /// Wider or taller than [`MAX_SIDE`], or a stream longer than [`MAX_PNG`].
    TooLarge,
}

/// The picture's size after a decode; the pixels are `width * height * 4`
/// bytes of RGBA in the buffer the caller passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    pub width: u32,
    pub height: u32,
}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Decode a PNG into `out` as RGBA, eight bits a channel, rows top to bottom.
///
/// `out` is cleared and filled; on a refusal its contents are unspecified
/// and the picture is not delivered. Colour type 2 (RGB) is widened with an
/// opaque alpha; colour type 6 (RGBA) is copied. Chunks the reader does not
/// need are skipped, and checksums are not verified: the bytes arrived on an
/// authenticated channel, and a picture that inflates and unfilters to the
/// declared size is what a checksum would have confirmed.
pub fn decode_png(png: &[u8], out: &mut Vec<u8>) -> Result<Decoded, Refusal> {
    if png.len() > MAX_PNG {
        return Err(Refusal::TooLarge);
    }
    let rest = png.strip_prefix(&SIGNATURE).ok_or(Refusal::NotPng)?;

    // The header chunk comes first and says what the picture is.
    let first = chunk(rest)?;
    let header: [u8; 13] = first.data.try_into().map_err(|_| Refusal::Malformed)?;
    if first.kind != *b"IHDR" {
        return Err(Refusal::Malformed);
    }
    let mut rest = first.rest;
    let width = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
    let height = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    let (depth, colour, compression, filter, interlace) =
        (header[8], header[9], header[10], header[11], header[12]);
    if width == 0 || height == 0 {
        return Err(Refusal::Malformed);
    }
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err(Refusal::TooLarge);
    }
    let channels = match (depth, colour, compression, filter, interlace) {
        (8, 2, 0, 0, 0) => 3usize,
        (8, 6, 0, 0, 0) => 4usize,
        _ => return Err(Refusal::Unsupported),
    };

    // The image data may be split across any number of chunks; they form one
    // stream, and the inflater takes them as they are.
    let mut idats: Vec<&[u8]> = Vec::new();
    loop {
        let next = chunk(rest)?;
        rest = next.rest;
        match &next.kind {
            b"IDAT" => idats.push(next.data),
            b"IEND" => break,
            _ => {}
        }
    }
    if idats.is_empty() {
        return Err(Refusal::Malformed);
    }

    // One filter byte, then the row, per row. The stream must inflate to
    // exactly that: fewer bytes is a truncated picture, more is not the
    // picture the header described.
    let (w, h) = (width as usize, height as usize);
    let stride = 1 + w * channels;
    let raw_len = h * stride;
    let mut raw = vec![0u8; raw_len];
    let inflated = miniz_oxide::inflate::decompress_slice_iter_to_slice(
        &mut raw,
        idats.iter().copied(),
        true,
        false,
    )
    .map_err(|_| Refusal::Malformed)?;
    if inflated != raw_len {
        return Err(Refusal::Malformed);
    }

    unfilter(&mut raw, stride, channels)?;

    out.clear();
    out.reserve(w * h * 4);
    for row in raw.chunks_exact(stride) {
        let pixels = row.get(1..).unwrap_or(&[]);
        if channels == 4 {
            out.extend_from_slice(pixels);
        } else {
            for rgb in pixels.chunks_exact(3) {
                out.extend_from_slice(rgb);
                out.push(0xFF);
            }
        }
    }
    Ok(Decoded { width, height })
}

/// Undo the per-row filters in place: each row's filter byte stays where it
/// is and the bytes after it become the pixels.
fn unfilter(raw: &mut [u8], stride: usize, bpp: usize) -> Result<(), Refusal> {
    let mut previous = vec![0u8; stride.saturating_sub(1)];
    let mut first = true;
    for row in raw.chunks_exact_mut(stride) {
        let (filter, pixels) = row.split_first_mut().ok_or(Refusal::Malformed)?;
        let above: &[u8] = if first { &[] } else { &previous };
        for i in 0..pixels.len() {
            let left = |at: usize| at.checked_sub(bpp);
            let a = left(i).and_then(|at| pixels.get(at)).copied().unwrap_or(0);
            let b = above.get(i).copied().unwrap_or(0);
            let c = left(i).and_then(|at| above.get(at)).copied().unwrap_or(0);
            let predicted = match *filter {
                0 => 0,
                1 => a,
                2 => b,
                // The mean of two bytes fits a byte.
                #[allow(clippy::cast_possible_truncation, reason = "a mean of two bytes")]
                3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                4 => paeth(a, b, c),
                _ => return Err(Refusal::Malformed),
            };
            if let Some(byte) = pixels.get_mut(i) {
                *byte = byte.wrapping_add(predicted);
            }
        }
        previous.copy_from_slice(pixels);
        first = false;
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// One chunk of the container: its type, its data, and what follows it.
struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
    rest: &'a [u8],
}

fn chunk(bytes: &[u8]) -> Result<Chunk<'_>, Refusal> {
    let len: [u8; 4] = bytes
        .get(..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(Refusal::Malformed)?;
    let kind: [u8; 4] = bytes
        .get(4..8)
        .and_then(|s| s.try_into().ok())
        .ok_or(Refusal::Malformed)?;
    let len = u32::from_be_bytes(len) as usize;
    let end = 8usize.checked_add(len).ok_or(Refusal::Malformed)?;
    let after = end.checked_add(4).ok_or(Refusal::Malformed)?;
    let data = bytes.get(8..end).ok_or(Refusal::Malformed)?;
    let rest = bytes.get(after..).ok_or(Refusal::Malformed)?;
    Ok(Chunk { kind, data, rest })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture of a known pattern, written by the encoder the host uses.
    fn picture(width: u32, height: u32) -> (Vec<u8>, Vec<u8>) {
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                pixels.extend_from_slice(&[
                    (x * 7) as u8,
                    (y * 13) as u8,
                    ((x ^ y) * 3) as u8,
                    if (x + y) % 2 == 0 { 0xFF } else { 0x80 },
                ]);
            }
        }
        let mut png = vec![0u8; lowlat_core::png::upper_bound(width, height)];
        let used = lowlat_core::png::encode(&pixels, width, height, (width * 4) as usize, &mut png)
            .unwrap();
        png.truncate(used);
        (png, pixels)
    }

    #[test]
    fn the_encoders_output_decodes_to_the_pixels_it_was_given() {
        let (png, pixels) = picture(37, 21);
        let mut out = Vec::new();
        let decoded = decode_png(&png, &mut out).unwrap();
        assert_eq!(
            decoded,
            Decoded {
                width: 37,
                height: 21
            }
        );
        assert_eq!(out, pixels);
    }

    /// The pictures an established host sent in a recorded session, with
    /// the pixels an independent decoder read out of each
    /// (`scripts/decode-cursor.py`). These are compressed where the encoder
    /// here writes stored blocks, so they are what exercises the inflate.
    #[test]
    fn the_pictures_a_host_sent_decode_to_the_independent_decoders_pixels() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("png") {
                continue;
            }
            let png = std::fs::read(&path).unwrap();
            let reference = std::fs::read(path.with_extension("rgba")).unwrap();
            let mut out = Vec::new();
            let decoded =
                decode_png(&png, &mut out).unwrap_or_else(|e| panic!("{}: {e:?}", path.display()));
            assert_eq!(
                (decoded.width * decoded.height * 4) as usize,
                reference.len(),
                "{}: size",
                path.display()
            );
            assert!(out == reference, "{}: pixels differ", path.display());
            seen += 1;
        }
        assert!(seen >= 6, "the committed pictures were not found: {seen}");
    }

    #[test]
    fn a_picture_at_the_ceiling_decodes_and_one_past_it_is_refused() {
        let (png, pixels) = picture(MAX_SIDE, MAX_SIDE);
        let mut out = Vec::new();
        decode_png(&png, &mut out).unwrap();
        assert_eq!(out.len(), MAX_DECODED);
        assert_eq!(out, pixels);

        let (png, _) = picture(MAX_SIDE + 1, 1);
        assert_eq!(decode_png(&png, &mut out), Err(Refusal::TooLarge));
    }

    #[test]
    fn what_is_not_a_pointer_picture_is_refused_without_a_panic() {
        let mut out = Vec::new();
        assert_eq!(decode_png(b"", &mut out), Err(Refusal::NotPng));
        assert_eq!(decode_png(b"GIF89a", &mut out), Err(Refusal::NotPng));
        assert_eq!(decode_png(&SIGNATURE, &mut out), Err(Refusal::Malformed));

        let (png, _) = picture(8, 8);
        // Truncated inside the image data.
        let cut = &png[..png.len() - 20];
        assert_eq!(decode_png(cut, &mut out), Err(Refusal::Malformed));

        // The header rewritten: a palette, sixteen bits, interlaced.
        for (at, value) in [(8 + 8 + 9, 3u8), (8 + 8 + 8, 16), (8 + 8 + 12, 1)] {
            let mut bent = png.clone();
            bent[at] = value;
            assert_eq!(decode_png(&bent, &mut out), Err(Refusal::Unsupported));
        }
        // A height that the stream does not cover.
        let mut taller = png.clone();
        taller[8 + 8 + 7] = 200;
        assert_eq!(decode_png(&taller, &mut out), Err(Refusal::Malformed));
        // A picture with no size.
        let mut empty = png.clone();
        empty[8 + 8 + 3] = 0;
        assert_eq!(decode_png(&empty, &mut out), Err(Refusal::Malformed));
    }

    #[test]
    fn the_five_filters_are_undone() {
        // Rows filtered by hand, each with a different filter, over a 2x5
        // RGB picture; the expected pixels are what the filters were applied
        // to. Sub uses the pixel to the left, Up the row above, Average their
        // mean, Paeth the predictor of the three.
        let rows: [[u8; 6]; 5] = [
            [10, 20, 30, 40, 50, 60],
            [11, 21, 31, 41, 51, 61],
            [12, 22, 32, 42, 52, 62],
            [13, 23, 33, 43, 53, 63],
            [14, 24, 34, 44, 54, 64],
        ];
        let mut raw = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let filter = index as u8;
            raw.push(filter);
            let above: Option<[u8; 6]> = index.checked_sub(1).map(|i| rows[i]);
            for i in 0..6 {
                let a = if i >= 3 { row[i - 3] } else { 0 };
                let b = above.map_or(0, |up| up[i]);
                let c = if i >= 3 {
                    above.map_or(0, |up| up[i - 3])
                } else {
                    0
                };
                let predicted = match filter {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((u16::from(a) + u16::from(b)) / 2) as u8,
                    _ => paeth(a, b, c),
                };
                raw.push(row[i].wrapping_sub(predicted));
            }
        }
        unfilter(&mut raw, 7, 3).unwrap();
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(&raw[index * 7 + 1..index * 7 + 7], row, "row {index}");
        }
    }

    #[test]
    fn the_cache_names_a_picture_and_forgets_on_request() {
        let mut cache = Cache::default();
        let (png, _) = picture(4, 4);
        let shape = Shape {
            width: 4,
            height: 4,
            hot_x: 1,
            hot_y: 2,
        };
        let checksum = cache.insert(shape, &png);
        assert_eq!(checksum, crc32::of(&png));
        assert_eq!(cache.get(checksum), Some((shape, png.as_slice())));
        assert_eq!(cache.get(checksum ^ 1), None);
        // The same picture again is one entry, not two.
        cache.insert(shape, &png);
        assert_eq!(cache.len(), 1);
        cache.clear();
        assert_eq!(cache.get(checksum), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn a_full_cache_starts_over() {
        let mut cache = Cache::default();
        let shape = Shape {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
        };
        let mut first = 0;
        for n in 0..CAPACITY {
            let (mut png, _) = picture(1, 1);
            // Distinct bytes per picture: a trailing byte past the end chunk
            // changes the checksum and nothing else.
            png.push(n as u8);
            let checksum = cache.insert(shape, &png);
            if n == 0 {
                first = checksum;
            }
        }
        assert_eq!(cache.len(), CAPACITY);
        assert!(cache.get(first).is_some());
        let (mut png, _) = picture(1, 1);
        png.extend_from_slice(b"one more");
        cache.insert(shape, &png);
        assert_eq!(cache.len(), 1);
        assert!(cache.get(first).is_none());
    }
}
