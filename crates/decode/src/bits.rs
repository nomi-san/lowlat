//! Reading the bit-level syntax a header is made of.
//!
//! The mirror of the encoders' writer: a big-endian bit reader with the
//! Exp-Golomb codes both coding standards use. **Emulation prevention is
//! removed as the bits are read**, so a header is parsed straight off the
//! unit with no unescaped copy: the reader watches for the two zero bytes
//! that precede an inserted `03` and steps over it.

use crate::ParseError;

type Result<T> = core::result::Result<T, ParseError>;

/// A big-endian bit reader over an escaped byte payload.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// The next byte to load.
    byte: usize,
    /// Bits left in `current`.
    left: u32,
    current: u8,
    /// Consecutive zero bytes seen, for the escape.
    zeros: u32,
    /// Bits consumed, escapes excluded.
    consumed: usize,
    /// The unescaped position of the last set bit, once looked for.
    last_set: Option<Option<usize>>,
    /// Escape bytes stepped over so far.
    escapes: u32,
}

/// The unescaped bit position of the payload's last set bit, if any.
fn last_set_bit(data: &[u8]) -> Option<usize> {
    let mut zeros = 0u32;
    let mut position = 0usize;
    let mut last = None;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        if b != 0 {
            last = Some(position + 7 - b.trailing_zeros() as usize);
        }
        position += 8;
    }
    last
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte: 0,
            left: 0,
            current: 0,
            zeros: 0,
            consumed: 0,
            last_set: None,
            escapes: 0,
        }
    }

    /// Bits read so far, escapes excluded.
    pub fn position(&self) -> usize {
        self.consumed
    }

    /// Escape bytes stepped over so far.
    pub fn escapes(&self) -> u32 {
        self.escapes
    }

    /// The offset of the next unread byte in the escaped payload, when the
    /// reader sits on a byte boundary. Slice data begins there.
    pub fn byte_offset(&self) -> usize {
        self.byte
    }

    fn load(&mut self) -> Result<()> {
        let mut next = *self.data.get(self.byte).ok_or(ParseError::Truncated)?;
        self.byte += 1;
        if self.zeros >= 2 && next == 0x03 {
            // The escape byte: skipped, and it resets the run.
            self.zeros = 0;
            self.escapes += 1;
            next = *self.data.get(self.byte).ok_or(ParseError::Truncated)?;
            self.byte += 1;
        }
        self.zeros = if next == 0 { self.zeros + 1 } else { 0 };
        self.current = next;
        self.left = 8;
        Ok(())
    }

    /// One bit.
    pub fn bit(&mut self) -> Result<bool> {
        if self.left == 0 {
            self.load()?;
        }
        self.left -= 1;
        self.consumed += 1;
        Ok((self.current >> self.left) & 1 == 1)
    }

    /// One flag, as the standards call a single bit.
    pub fn flag(&mut self) -> Result<bool> {
        self.bit()
    }

    /// `count` bits, up to 32, as an unsigned value.
    pub fn bits(&mut self, count: u32) -> Result<u32> {
        if count > 32 {
            return Err(ParseError::OutOfRange);
        }
        let mut value: u32 = 0;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.bit()?);
        }
        Ok(value)
    }

    /// Up to 8 bits, as a byte.
    pub fn u8(&mut self, count: u32) -> Result<u8> {
        if count > 8 {
            return Err(ParseError::OutOfRange);
        }
        u8::try_from(self.bits(count)?).map_err(|_| ParseError::OutOfRange)
    }

    /// An unsigned Exp-Golomb code.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while !self.bit()? {
            zeros += 1;
            if zeros > 31 {
                return Err(ParseError::OutOfRange);
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.bits(zeros)?;
        ((1u32 << zeros) - 1)
            .checked_add(rest)
            .ok_or(ParseError::OutOfRange)
    }

    /// An unsigned Exp-Golomb code bounded by what the syntax allows.
    pub fn ue_max(&mut self, max: u32) -> Result<u32> {
        let value = self.ue()?;
        if value > max {
            return Err(ParseError::OutOfRange);
        }
        Ok(value)
    }

    /// A signed Exp-Golomb code.
    pub fn se(&mut self) -> Result<i32> {
        let code = self.ue()?;
        let magnitude = i32::try_from(code.div_ceil(2)).map_err(|_| ParseError::OutOfRange)?;
        Ok(if code & 1 == 1 { magnitude } else { -magnitude })
    }

    /// Whether more syntax follows before the trailing bits.
    ///
    /// True while any bit but the last set one in the payload remains: that
    /// last set bit is the stop bit and everything after it is alignment.
    /// The payload is scanned for it once, on the first call, so a slice
    /// header (which never asks) costs no pass over its data.
    pub fn more_rbsp_data(&mut self) -> bool {
        let last = match self.last_set {
            Some(last) => last,
            None => {
                let last = last_set_bit(self.data);
                self.last_set = Some(last);
                last
            }
        };
        last.is_some_and(|last| self.consumed < last)
    }

    /// Skip to the next byte boundary.
    pub fn align(&mut self) {
        self.consumed += self.left as usize;
        self.left = 0;
    }

    /// Skip `count` bits.
    pub fn skip(&mut self, count: u32) -> Result<()> {
        for _ in 0..count {
            self.bit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_reads_what_the_writer_writes() {
        // ue: 0 -> 1, 1 -> 010, 2 -> 011, 3 -> 00100, 7 -> 0001000
        // Packed: 1 010 011 00100 0001000 -> 1010 0110 0100 0001 000(pad)
        let data = [0b1010_0110, 0b0100_0001, 0b0000_0000];
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 1);
        assert_eq!(r.ue().unwrap(), 2);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 7);
    }

    #[test]
    fn signed_codes_alternate_sign() {
        // se: 1 -> 010 (+1), 2 -> 011 (-1), 3 -> 00100 (+2), 4 -> 00101 (-2)
        let data = [0b0100_1100, 0b1000_0101];
        let mut r = BitReader::new(&data);
        assert_eq!(r.se().unwrap(), 1);
        assert_eq!(r.se().unwrap(), -1);
        assert_eq!(r.se().unwrap(), 2);
        assert_eq!(r.se().unwrap(), -2);
    }

    #[test]
    fn the_escape_byte_is_skipped() {
        // 00 00 03 01: the 03 is an escape, so the bits read are 00 00 01.
        let data = [0x00, 0x00, 0x03, 0x01];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(8).unwrap(), 0);
        assert_eq!(r.bits(8).unwrap(), 0);
        assert_eq!(r.bits(8).unwrap(), 1);
        assert!(r.bit().is_err());
    }

    #[test]
    fn a_zero_run_that_was_reset_does_not_escape() {
        // 00 00 03 03: the first 03 is an escape (skipped); the second is a
        // plain byte because the run was reset.
        let data = [0x00, 0x00, 0x03, 0x03];
        let mut r = BitReader::new(&data);
        assert_eq!(r.bits(16).unwrap(), 0);
        assert_eq!(r.bits(8).unwrap(), 3);
    }

    #[test]
    fn more_data_stops_at_the_stop_bit() {
        // 1 bit of data, then the stop bit, then padding: 1 1 000000.
        let data = [0b1100_0000];
        let mut r = BitReader::new(&data);
        assert!(r.more_rbsp_data());
        assert!(r.bit().unwrap());
        assert!(!r.more_rbsp_data());
        // Data continues in a later byte.
        let data = [0b1000_0000, 0b1100_0000];
        let mut r = BitReader::new(&data);
        assert!(r.more_rbsp_data());
        r.bit().unwrap();
        assert!(r.more_rbsp_data());
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        let mut r = BitReader::new(&[]);
        assert_eq!(r.bit(), Err(ParseError::Truncated));
        assert_eq!(r.ue(), Err(ParseError::Truncated));
        let mut r = BitReader::new(&[0]);
        assert_eq!(r.ue(), Err(ParseError::Truncated));
    }
}
