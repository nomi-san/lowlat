//! CRC-32, the reflected polynomial.
//!
//! **Not a cryptographic primitive and not on a hot path.** It checks
//! connectivity messages a few times a second, seals the chunks of a cursor
//! image, and keys the cache a peer holds those images in. A table would buy
//! nothing at those rates and costs a kilobyte.

/// A running checksum.
#[derive(Debug, Clone, Copy)]
pub struct Crc32(u32);

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    pub const fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    pub const fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

/// The checksum of one run of bytes.
pub fn of(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published vector for this polynomial.
    #[test]
    fn the_standard_vector() {
        assert_eq!(of(b"123456789"), 0xCBF4_3926);
    }

    /// Feeding it in pieces is the same as feeding it whole, which the chunked
    /// writers below depend on.
    #[test]
    fn splitting_the_input_changes_nothing() {
        let whole = of(b"the quick brown fox");
        let mut split = Crc32::new();
        split.update(b"the quick ");
        split.update(b"brown fox");
        assert_eq!(whole, split.finish());
    }
}
