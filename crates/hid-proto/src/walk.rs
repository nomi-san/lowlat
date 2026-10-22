//! A reader of report descriptors, for the tests: each main item becomes the
//! field it declares, at its bit offset in its report, so a descriptor can be
//! checked against the code that writes its reports and the values a report
//! holds read back through it.

use std::collections::BTreeMap;
use std::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Report {
    Input,
    Output,
    Feature,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) report: Report,
    pub(crate) id: u8,
    /// The field's first bit, counted after the identifier byte.
    pub(crate) offset: u32,
    pub(crate) size: u32,
    pub(crate) count: u32,
    /// Usages named one by one, page in the high word.
    pub(crate) usages: Vec<u32>,
    pub(crate) usage_min: Option<u32>,
    pub(crate) usage_max: Option<u32>,
    pub(crate) flags: u32,
    pub(crate) logical_min: i32,
    pub(crate) logical_max: i32,
    pub(crate) physical_min: i32,
    pub(crate) physical_max: i32,
}

impl Field {
    pub(crate) fn constant(&self) -> bool {
        self.flags & 0x01 != 0
    }

    pub(crate) fn variable(&self) -> bool {
        self.flags & 0x02 != 0
    }

    pub(crate) fn relative(&self) -> bool {
        self.flags & 0x04 != 0
    }

    pub(crate) fn null_state(&self) -> bool {
        self.flags & 0x40 != 0
    }

    /// The usage page of the field's usages.
    pub(crate) fn page(&self) -> u32 {
        self.usages
            .first()
            .copied()
            .or(self.usage_min)
            .map_or(0, |usage| usage >> 16)
    }

    /// The element that carries `usage`, if the field names it.
    pub(crate) fn element(&self, usage: u32) -> Option<u32> {
        if let Some(at) = self.usages.iter().position(|u| *u == usage) {
            return u32::try_from(at).ok();
        }
        match (self.usage_min, self.usage_max) {
            (Some(min), Some(max)) if (min..=max).contains(&usage) && self.variable() => {
                Some(usage - min)
            }
            _ => None,
        }
    }

    /// Whether the field names `usage`, one by one or within its range.
    pub(crate) fn names(&self, usage: u32) -> bool {
        self.usages.contains(&usage)
            || matches!((self.usage_min, self.usage_max), (Some(min), Some(max)) if (min..=max).contains(&usage))
    }

    /// Element `element`'s value in `report`, identifier byte first, as the
    /// field's bits read little-endian; signed where the logical minimum is
    /// below zero.
    pub(crate) fn value(&self, report: &[u8], element: u32) -> i32 {
        let start = self.offset + element * self.size;
        let mut raw: u64 = 0;
        for bit in 0..self.size {
            let at = start + bit;
            let byte = report[1 + usize::try_from(at / 8).unwrap()];
            if byte & (1 << (at % 8)) != 0 {
                raw |= 1 << bit;
            }
        }
        if self.logical_min < 0 && self.size < 32 && raw & (1 << (self.size - 1)) != 0 {
            i32::try_from(raw as i64 - (1i64 << self.size)).unwrap()
        } else {
            i32::try_from(raw).unwrap()
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Globals {
    page: u32,
    logical_min: i32,
    logical_max: i32,
    physical_min: i32,
    physical_max: i32,
    size: u32,
    count: u32,
    id: u8,
}

/// Every field `descriptor` declares, in order; an error for a descriptor
/// that does not read whole.
pub(crate) fn walk(descriptor: &[u8]) -> Result<Vec<Field>, &'static str> {
    let mut fields = Vec::new();
    let mut globals = Globals::default();
    let mut pushed: Vec<Globals> = Vec::new();
    let mut usages: Vec<u32> = Vec::new();
    let (mut usage_min, mut usage_max) = (None, None);
    let mut depth = 0i32;
    let mut offsets: BTreeMap<(Report, u8), u32> = BTreeMap::new();
    let mut at = 0;
    while at < descriptor.len() {
        let prefix = descriptor[at];
        if prefix == 0xFE {
            let len = usize::from(*descriptor.get(at + 1).ok_or("a long item cut short")?);
            at += 3 + len;
            continue;
        }
        let len = match prefix & 0x03 {
            3 => 4,
            n => usize::from(n),
        };
        let data = descriptor
            .get(at + 1..at + 1 + len)
            .ok_or("an item cut short")?;
        let unsigned = data
            .iter()
            .rev()
            .fold(0u32, |value, byte| (value << 8) | u32::from(*byte));
        let signed = match len {
            0 => 0,
            1 => i32::from(data[0] as i8),
            2 => i32::from(i16::from_le_bytes([data[0], data[1]])),
            _ => unsigned as i32,
        };
        let usage = |page: u32| {
            if len == 4 {
                unsigned
            } else {
                (page << 16) | unsigned
            }
        };
        match ((prefix >> 2) & 0x03, prefix >> 4) {
            (0, tag @ (8 | 9 | 11)) => {
                let report = match tag {
                    8 => Report::Input,
                    9 => Report::Output,
                    _ => Report::Feature,
                };
                let key = (report, globals.id);
                let offset = offsets.get(&key).copied().unwrap_or(0);
                fields.push(Field {
                    report,
                    id: globals.id,
                    offset,
                    size: globals.size,
                    count: globals.count,
                    usages: core::mem::take(&mut usages),
                    usage_min: usage_min.take(),
                    usage_max: usage_max.take(),
                    flags: unsigned,
                    logical_min: globals.logical_min,
                    logical_max: globals.logical_max,
                    physical_min: globals.physical_min,
                    physical_max: globals.physical_max,
                });
                offsets.insert(key, offset + globals.size * globals.count);
            }
            (0, 10) => {
                depth += 1;
                usages.clear();
                usage_min = None;
                usage_max = None;
            }
            (0, 12) => {
                depth -= 1;
                if depth < 0 {
                    return Err("an end with no collection");
                }
            }
            (1, 0) => globals.page = unsigned,
            (1, 1) => globals.logical_min = signed,
            (1, 2) => {
                globals.logical_max = if globals.logical_min >= 0 {
                    unsigned as i32
                } else {
                    signed
                }
            }
            (1, 3) => globals.physical_min = signed,
            (1, 4) => globals.physical_max = signed,
            (1, 7) => globals.size = unsigned,
            (1, 8) => {
                globals.id =
                    u8::try_from(unsigned).map_err(|_| "a report identifier past a byte")?
            }
            (1, 9) => globals.count = unsigned,
            (1, 10) => pushed.push(globals),
            (1, 11) => globals = pushed.pop().ok_or("a pop with nothing pushed")?,
            (2, 0) => usages.push(usage(globals.page)),
            (2, 1) => usage_min = Some(usage(globals.page)),
            (2, 2) => usage_max = Some(usage(globals.page)),
            _ => {}
        }
        at += 1 + len;
    }
    if depth != 0 {
        return Err("a collection never ended");
    }
    Ok(fields)
}

/// A report's length in bytes, its identifier byte included where it has one.
pub(crate) fn report_len(fields: &[Field], report: Report, id: u8) -> usize {
    let bits: u32 = fields
        .iter()
        .filter(|f| f.report == report && f.id == id)
        .map(|f| f.size * f.count)
        .sum();
    usize::try_from(bits.div_ceil(8)).unwrap() + usize::from(id != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_that_does_not_read_whole_is_an_error() {
        assert!(walk(&[0xA1, 0x01]).is_err(), "a collection never ended");
        assert!(walk(&[0xC0]).is_err(), "an end with no collection");
        assert!(walk(&[0x26, 0xFF]).is_err(), "an item cut short");
        assert!(walk(&[0xB4]).is_err(), "a pop with nothing pushed");
    }

    #[test]
    fn fields_follow_one_another_within_their_report() {
        let fields = walk(&[
            0x85, 0x03, 0x75, 0x04, 0x95, 0x02, 0x81, 0x02, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02,
        ])
        .unwrap();
        assert_eq!((fields[0].offset, fields[1].offset), (0, 8));
        assert_eq!(report_len(&fields, Report::Input, 3), 3);
        let field = Field {
            logical_min: -127,
            logical_max: 127,
            ..fields[1].clone()
        };
        assert_eq!(field.value(&[3, 0x00, 0xFF], 0), -1);
    }
}
