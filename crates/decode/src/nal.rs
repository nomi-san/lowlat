//! Walking an access unit's units.
//!
//! An access unit on the wire is byte-stream framed: each unit is led by a
//! start code of two or three zero bytes and a one, and ends where the next
//! start code begins (a zero byte immediately ahead of a start code belongs
//! to the framing, not the unit).

/// One unit: its bytes from the header byte to the end, escapes included,
/// and where it sits in the access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unit<'a> {
    pub bytes: &'a [u8],
    /// Offset of the first byte of `bytes` in the access unit.
    pub offset: usize,
}

/// The units of an access unit, in order.
#[derive(Debug, Clone)]
pub struct Units<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Units<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, at: 0 }
    }
}

/// Where the next `00 00 01` begins at or after `from`.
fn next_start_code(data: &[u8], from: usize) -> Option<usize> {
    let tail = data.get(from..)?;
    tail.windows(3)
        .position(|w| w == [0, 0, 1])
        .map(|at| from + at)
}

impl<'a> Iterator for Units<'a> {
    type Item = Unit<'a>;

    fn next(&mut self) -> Option<Unit<'a>> {
        let start_code = next_start_code(self.data, self.at)?;
        let begin = start_code + 3;
        let end = match next_start_code(self.data, begin) {
            Some(next) => {
                // A trailing zero ahead of the next start code is framing.
                if next > begin && self.data.get(next - 1) == Some(&0) {
                    next - 1
                } else {
                    next
                }
            }
            None => self.data.len(),
        };
        self.at = end;
        let bytes = self.data.get(begin..end)?;
        if bytes.is_empty() {
            return self.next();
        }
        Some(Unit {
            bytes,
            offset: begin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_are_split_at_start_codes_with_the_framing_zero_dropped() {
        let data = [
            0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x68, 0xBB, 0xCC, 0, 0, 0, 1, 0x65, 0xDD, 0,
        ];
        let units: Vec<_> = Units::new(&data).collect();
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].bytes, &[0x67, 0xAA]);
        assert_eq!(units[0].offset, 4);
        assert_eq!(units[1].bytes, &[0x68, 0xBB, 0xCC]);
        // The last unit keeps its own trailing zero: nothing follows it.
        assert_eq!(units[2].bytes, &[0x65, 0xDD, 0]);
    }

    #[test]
    fn no_start_code_means_no_units() {
        assert_eq!(Units::new(&[0x67, 0xAA]).count(), 0);
        assert_eq!(Units::new(&[]).count(), 0);
        // An empty unit between two start codes is skipped.
        assert_eq!(Units::new(&[0, 0, 1, 0, 0, 1, 0x41]).count(), 1);
    }
}
