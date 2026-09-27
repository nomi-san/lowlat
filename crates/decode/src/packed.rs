//! The packed full-chroma layouts two device interfaces decode into, taken
//! apart into the three planes every backend hands out.

/// One row of the packed eight-bit full-chroma layout -- V, U, Y and a
/// fourth byte per sample, in that order in memory -- into three planes.
pub(crate) fn unpack_vuyx_row(from: &[u8], y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    for (((px, py), pu), pv) in from
        .chunks_exact(4)
        .zip(y.iter_mut())
        .zip(u.iter_mut())
        .zip(v.iter_mut())
    {
        if let [sv, su, sy, _] = px {
            *py = *sy;
            *pu = *su;
            *pv = *sv;
        }
    }
}

/// One row of the packed ten-bit full-chroma layout -- a little-endian word
/// per sample with U in its low ten bits, then Y, then V, then two bits
/// unused -- into three planes of sixteen-bit samples with the value in the
/// high ten bits, native order.
pub(crate) fn unpack_y410_row(from: &[u8], y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    for (((px, py), pu), pv) in from
        .chunks_exact(4)
        .zip(y.chunks_exact_mut(2))
        .zip(u.chunks_exact_mut(2))
        .zip(v.chunks_exact_mut(2))
    {
        if let [a, b, c, d] = px {
            let word = u32::from_le_bytes([*a, *b, *c, *d]);
            // Ten bits masked out of the word fit sixteen with the shift.
            let ten = |shift: u32| u16::try_from((word >> shift) & 0x3ff).unwrap_or(0) << 6;
            py.copy_from_slice(&ten(10).to_ne_bytes());
            pu.copy_from_slice(&ten(0).to_ne_bytes());
            pv.copy_from_slice(&ten(20).to_ne_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packed layouts unpacked against the same thing written plainly:
    /// eight-bit samples in the order V, U, Y, X per word, and ten-bit ones
    /// with U in a word's low bits, then Y, then V, moved to the high bits
    /// of sixteen on the way out.
    #[test]
    fn the_packed_layouts_unpack_to_the_planes() {
        let width = 13usize;
        let mut packed = Vec::with_capacity(4 * width);
        for i in 0..width {
            let (y, u, v) = ((i * 7) as u8, (i * 11 + 3) as u8, (i * 13 + 5) as u8);
            packed.extend_from_slice(&[v, u, y, 0xff]);
        }
        let (mut y, mut u, mut v) = (vec![0u8; width], vec![0u8; width], vec![0u8; width]);
        unpack_vuyx_row(&packed, &mut y, &mut u, &mut v);
        for i in 0..width {
            assert_eq!(y[i], (i * 7) as u8, "y {i}");
            assert_eq!(u[i], (i * 11 + 3) as u8, "u {i}");
            assert_eq!(v[i], (i * 13 + 5) as u8, "v {i}");
        }

        let mut packed = Vec::with_capacity(4 * width);
        let sample = |i: usize, k: usize| ((i * k + 1) % 1024) as u32;
        for i in 0..width {
            let word = sample(i, 79) | (sample(i, 37) << 10) | (sample(i, 53) << 20) | (3 << 30);
            packed.extend_from_slice(&word.to_le_bytes());
        }
        let (mut y, mut u, mut v) = (
            vec![0u8; 2 * width],
            vec![0u8; 2 * width],
            vec![0u8; 2 * width],
        );
        unpack_y410_row(&packed, &mut y, &mut u, &mut v);
        for i in 0..width {
            let read = |p: &[u8]| u16::from_ne_bytes([p[2 * i], p[2 * i + 1]]);
            assert_eq!(read(&u), (sample(i, 79) << 6) as u16, "u {i}");
            assert_eq!(read(&y), (sample(i, 37) << 6) as u16, "y {i}");
            assert_eq!(read(&v), (sample(i, 53) << 6) as u16, "v {i}");
            assert_eq!(read(&y) & 0x3f, 0, "the low bits are clear");
        }
    }
}
