//! Writing the bit-level syntax a parameter set is made of.
//!
//! Needed because one backend has nowhere to put the colour description. The
//! other takes it as structure fields; this interface's sequence parameters
//! carry only an aspect-ratio flag, so the only way to state BT.709 and limited
//! range ([05 §3.1](../../../docs/05-host.md)) is to write the parameter set
//! ourselves and hand it over as a packed header.
//!
//! Everything here is fixed by the coding standard rather than by any vendor,
//! which is why it is testable without hardware and why the tests use values
//! computed by hand rather than captured from a device.

/// A big-endian bit writer over caller storage.
///
/// Borrows rather than owns, in keeping with the rest of the project: the
/// buffer is allocated once where the encoder is built.
#[derive(Debug)]
pub struct BitWriter<'a> {
    out: &'a mut [u8],
    /// Bits written, so the byte is `bits / 8` and the shift is `bits % 8`.
    bits: usize,
}

impl<'a> BitWriter<'a> {
    pub fn new(out: &'a mut [u8]) -> Self {
        out.fill(0);
        Self { out, bits: 0 }
    }

    /// Bits written so far.
    pub fn bit_len(&self) -> usize {
        self.bits
    }

    /// The written prefix, whole bytes only.
    ///
    /// A caller that has not reached a byte boundary has not finished writing
    /// a parameter set, because the trailing bits are what pad it to one.
    pub fn finish(self) -> &'a [u8] {
        let bytes = self.bits.div_ceil(8);
        self.out.get(..bytes).unwrap_or(&[])
    }

    /// One bit.
    pub fn bit(&mut self, set: bool) -> bool {
        let index = self.bits / 8;
        let Some(byte) = self.out.get_mut(index) else {
            return false;
        };
        if set {
            // Most significant bit first, which is the order the standard
            // reads them back in.
            *byte |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
        true
    }

    /// `count` bits of `value`, most significant first.
    pub fn bits(&mut self, value: u32, count: u32) -> bool {
        for shift in (0..count).rev() {
            if !self.bit((value >> shift) & 1 == 1) {
                return false;
            }
        }
        true
    }

    /// Unsigned exponential-Golomb.
    ///
    /// `value + 1` in binary, with one fewer leading zero than it has bits. So
    /// zero is a single set bit and the encoding grows two bits per doubling.
    pub fn ue(&mut self, value: u32) -> bool {
        let shifted = value.saturating_add(1);
        let significant = 32 - shifted.leading_zeros();
        // The leading zeros, then the value itself including its top bit.
        if !self.bits(0, significant - 1) {
            return false;
        }
        self.bits(shifted, significant)
    }

    /// Signed exponential-Golomb.
    ///
    /// Folded onto the unsigned form: zero maps to zero, then positive and
    /// negative alternate. **Negative maps one lower than the mirror**, which
    /// is why -1 and 1 are not symmetric.
    pub fn se(&mut self, value: i32) -> bool {
        // Folded without a signed cast: the magnitude is taken first, so
        // nothing here can turn a negative into an enormous positive.
        let magnitude = value.unsigned_abs();
        let folded = if value <= 0 {
            magnitude.saturating_mul(2)
        } else {
            magnitude.saturating_mul(2).saturating_sub(1)
        };
        self.ue(folded)
    }

    /// The stop bit and padding that end a raw byte sequence payload.
    pub fn trailing_bits(&mut self) -> bool {
        if !self.bit(true) {
            return false;
        }
        while self.bits % 8 != 0 {
            if !self.bit(false) {
                return false;
            }
        }
        true
    }
}

/// Copy `payload` into `out`, inserting emulation prevention.
///
/// A payload is not allowed to contain a start code, so any run of two zero
/// bytes followed by a byte below four gets a `0x03` inserted before that byte.
/// **Omitting this produces a stream that decodes correctly until the day the
/// payload happens to contain the pattern**, which is a fault that appears with
/// content rather than with code and is therefore very hard to attribute.
///
/// Returns the number of bytes written, or `None` if `out` is too small.
pub fn escape(payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut written = 0usize;
    let mut zeros = 0usize;
    for &byte in payload {
        if zeros >= 2 && byte <= 0x03 {
            *out.get_mut(written)? = 0x03;
            written += 1;
            zeros = 0;
        }
        *out.get_mut(written)? = byte;
        written += 1;
        if byte == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    Some(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a written prefix back as a string of bits, so the expectations
    /// below can be the literal codes from the standard rather than hex nobody
    /// can check by eye.
    fn bits_of(bytes: &[u8], count: usize) -> String {
        (0..count)
            .map(|index| {
                let byte = bytes[index / 8];
                if byte & (0x80 >> (index % 8)) != 0 {
                    '1'
                } else {
                    '0'
                }
            })
            .collect()
    }

    fn write_ue(value: u32) -> String {
        let mut buffer = [0u8; 16];
        let mut writer = BitWriter::new(&mut buffer);
        assert!(writer.ue(value));
        let count = writer.bit_len();
        let bytes = writer.finish();
        bits_of(bytes, count)
    }

    fn write_se(value: i32) -> String {
        let mut buffer = [0u8; 16];
        let mut writer = BitWriter::new(&mut buffer);
        assert!(writer.se(value));
        let count = writer.bit_len();
        let bytes = writer.finish();
        bits_of(bytes, count)
    }

    /// The codes are fixed by the standard, so these are the standard's own
    /// table rather than anything observed from a device.
    #[test]
    fn unsigned_codes_match_the_standard() {
        assert_eq!(write_ue(0), "1");
        assert_eq!(write_ue(1), "010");
        assert_eq!(write_ue(2), "011");
        assert_eq!(write_ue(3), "00100");
        assert_eq!(write_ue(4), "00101");
        assert_eq!(write_ue(5), "00110");
        assert_eq!(write_ue(6), "00111");
        assert_eq!(write_ue(7), "0001000");
        assert_eq!(write_ue(8), "0001001");
    }

    /// Signed folds onto unsigned with positive and negative alternating, and
    /// the asymmetry at one is the part worth pinning: -1 is not the mirror of
    /// 1.
    #[test]
    fn signed_codes_match_the_standard() {
        assert_eq!(write_se(0), "1");
        assert_eq!(write_se(1), "010");
        assert_eq!(write_se(-1), "011");
        assert_eq!(write_se(2), "00100");
        assert_eq!(write_se(-2), "00101");
        assert_eq!(write_se(3), "00110");
        assert_eq!(write_se(-3), "00111");
    }

    #[test]
    fn fixed_width_fields_are_most_significant_first() {
        let mut buffer = [0u8; 4];
        let mut writer = BitWriter::new(&mut buffer);
        // 0x67 is the byte that opens a parameter set, and reading it back in
        // the wrong bit order gives 0xE6.
        assert!(writer.bits(0x67, 8));
        assert_eq!(writer.finish(), &[0x67]);
    }

    #[test]
    fn trailing_bits_stop_then_pad_to_a_byte() {
        let mut buffer = [0u8; 4];
        let mut writer = BitWriter::new(&mut buffer);
        assert!(writer.bits(0b101, 3));
        assert!(writer.trailing_bits());
        assert_eq!(writer.bit_len(), 8);
        // Three payload bits, the stop bit, then zeros.
        assert_eq!(writer.finish(), &[0b1011_0000]);
    }

    /// A writer that silently truncates would corrupt a parameter set rather
    /// than refuse to produce one, so running out of room is reported.
    #[test]
    fn a_full_buffer_refuses_rather_than_truncating() {
        let mut buffer = [0u8; 1];
        let mut writer = BitWriter::new(&mut buffer);
        assert!(writer.bits(0xFF, 8));
        assert!(!writer.bit(true), "wrote past the end of the buffer");
    }

    /// The pattern this exists to prevent is a start code appearing inside a
    /// payload. All four trailing bytes below four must be escaped.
    #[test]
    fn escaping_covers_every_byte_a_start_code_can_end_with() {
        let mut out = [0u8; 32];
        for tail in 0..=0x03u8 {
            let n = escape(&[0x00, 0x00, tail], &mut out).expect("room");
            assert_eq!(
                &out[..n],
                &[0x00, 0x00, 0x03, tail],
                "byte {tail:#04x} after two zeros was not escaped"
            );
        }
        // A byte above three needs no escape.
        let n = escape(&[0x00, 0x00, 0x04], &mut out).expect("room");
        assert_eq!(&out[..n], &[0x00, 0x00, 0x04]);
    }

    /// The zero run restarts after an inserted byte, so a long run of zeros
    /// gets an escape every other pair rather than one for the whole run.
    #[test]
    fn a_long_zero_run_is_escaped_repeatedly() {
        let mut out = [0u8; 32];
        let n = escape(&[0, 0, 0, 0, 0], &mut out).expect("room");
        assert_eq!(&out[..n], &[0, 0, 3, 0, 0, 3, 0]);
    }

    #[test]
    fn escaping_reports_a_buffer_that_cannot_hold_the_result() {
        let mut out = [0u8; 3];
        assert!(escape(&[0x00, 0x00, 0x01], &mut out).is_none());
    }
}
