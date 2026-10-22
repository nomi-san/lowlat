//! The keyboard: a bitmap of the keyboard page with its eight modifiers, so
//! no number of keys held at once is too many, and one consumer usage for
//! mute, volume and the media keys (docs/11-drivers.md section 2.3). No system
//! control collection and no lock lights, and the keyboard page's power key is
//! never set: the power, sleep and wake keys stay out of a guest's reach.

/// The keys: the modifiers, then one bit per usage 0x00 to 0xDF.
pub const KEYS_REPORT_ID: u8 = 1;
pub const KEYS_REPORT_LEN: usize = 30;
/// One consumer usage, 0 while none is held.
pub const CONSUMER_REPORT_ID: u8 = 2;
pub const CONSUMER_REPORT_LEN: usize = 3;

/// The highest usage the bitmap carries; the modifiers, 0xE0 to 0xE7, are the
/// report's first byte after its identifier.
pub const LAST_KEY: u8 = 0xDF;
/// The keyboard page's power key, which no report sets.
pub const POWER: u8 = 0x66;
/// The highest consumer usage the report carries.
pub const LAST_CONSUMER: u16 = 0x3FF;

const FIRST_MODIFIER: u8 = 0xE0;
const LAST_MODIFIER: u8 = 0xE7;
/// The first usage that is a key: 0x00 to 0x03 are the page's error codes.
const FIRST_KEY: u8 = 0x04;
/// Where the bitmap starts: after the identifier and the modifiers.
const BITMAP_AT: usize = 2;

pub const DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x06, // Usage (Keyboard)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x05, 0x07, //   Usage Page (Keyboard)
    0x19, 0xE0, //   Usage Minimum (Left Control)
    0x29, 0xE7, //   Usage Maximum (Right GUI)
    0x15, 0x00, //   Logical Minimum (0)
    0x25, 0x01, //   Logical Maximum (1)
    0x75, 0x01, //   Report Size (1)
    0x95, 0x08, //   Report Count (8)
    0x81, 0x02, //   Input (Data, Variable, Absolute)
    0x19, 0x00, //   Usage Minimum (0)
    0x29, 0xDF, //   Usage Maximum (0xDF)
    0x96, 0xE0, 0x00, //   Report Count (224)
    0x81, 0x02, //   Input (Data, Variable, Absolute)
    0xC0, // End Collection
    0x05, 0x0C, // Usage Page (Consumer)
    0x09, 0x01, // Usage (Consumer Control)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x02, //   Report ID (2)
    0x19, 0x00, //   Usage Minimum (0)
    0x2A, 0xFF, 0x03, //   Usage Maximum (0x3FF)
    0x15, 0x00, //   Logical Minimum (0)
    0x26, 0xFF, 0x03, //   Logical Maximum (0x3FF)
    0x75, 0x10, //   Report Size (16)
    0x95, 0x01, //   Report Count (1)
    0x81, 0x00, //   Input (Data, Array, Absolute)
    0xC0, // End Collection
];

/// No key down.
pub const NEUTRAL_KEYS: [u8; KEYS_REPORT_LEN] = Keys::new().report;
/// No consumer usage held.
pub const NEUTRAL_CONSUMER: [u8; CONSUMER_REPORT_LEN] = [CONSUMER_REPORT_ID, 0, 0];

/// The keys held, as the keyboard report carries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keys {
    report: [u8; KEYS_REPORT_LEN],
}

impl Default for Keys {
    fn default() -> Self {
        Self::new()
    }
}

impl Keys {
    /// Nothing held.
    pub const fn new() -> Self {
        let mut report = [0u8; KEYS_REPORT_LEN];
        report[0] = KEYS_REPORT_ID;
        Self { report }
    }

    /// The byte and the bit a usage takes, the modifiers in the first byte
    /// after the identifier.
    const fn bit(usage: u8) -> Option<(usize, u8)> {
        match usage {
            FIRST_MODIFIER..=LAST_MODIFIER => Some((1, 1 << (usage - FIRST_MODIFIER))),
            POWER => None,
            FIRST_KEY..=LAST_KEY => Some((BITMAP_AT + (usage / 8) as usize, 1 << (usage % 8))),
            _ => None,
        }
    }

    /// A key down or up, by its usage on the keyboard page. False, and nothing
    /// changed, for a usage the report does not carry -- the page's error
    /// codes and anything above the modifiers -- and for the power key.
    pub fn set(&mut self, usage: u8, down: bool) -> bool {
        let Some((at, mask)) = Self::bit(usage) else {
            return false;
        };
        let Some(byte) = self.report.get_mut(at) else {
            return false;
        };
        if down {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
        true
    }

    pub fn is_down(&self, usage: u8) -> bool {
        Self::bit(usage)
            .is_some_and(|(at, mask)| self.report.get(at).is_some_and(|byte| byte & mask != 0))
    }

    /// Whether any key or modifier is down.
    pub fn any(&self) -> bool {
        self.report
            .get(1..)
            .is_some_and(|held| held.iter().any(|byte| *byte != 0))
    }

    pub const fn report(&self) -> &[u8; KEYS_REPORT_LEN] {
        &self.report
    }
}

/// The consumer report holding `usage`, 0 for none; none above the report's
/// range.
pub const fn consumer_report(usage: u16) -> Option<[u8; CONSUMER_REPORT_LEN]> {
    if usage > LAST_CONSUMER {
        return None;
    }
    let [low, high] = usage.to_le_bytes();
    Some([CONSUMER_REPORT_ID, low, high])
}

/// Whether a keyboard report may reach the system: the power key's bit is
/// clear. Whoever wrote the report, the driver checks this before submitting
/// it.
pub fn keys_allowed(report: &[u8]) -> bool {
    report
        .get(BITMAP_AT + usize::from(POWER / 8))
        .is_some_and(|byte| byte & (1 << (POWER % 8)) == 0)
}

/// Whether a consumer report names a usage the report carries.
pub fn consumer_allowed(report: &[u8]) -> bool {
    match report {
        [_, low, high] => u16::from_le_bytes([*low, *high]) <= LAST_CONSUMER,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{Report, walk};
    use std::vec::Vec;

    #[test]
    fn a_key_is_its_usages_bit_and_a_modifier_the_first_byte() {
        let mut keys = Keys::new();
        assert!(!keys.any());
        assert!(keys.set(0x04, true), "A");
        assert!(keys.set(LAST_KEY, true));
        assert!(keys.set(0xE1, true), "left shift");
        let report = keys.report();
        assert_eq!(report[0], KEYS_REPORT_ID);
        assert_eq!(report[1], 0b0000_0010, "left shift is the second modifier");
        assert_eq!(report[2], 1 << 4, "usage 0x04 is the bitmap's fifth bit");
        assert_eq!(report[29], 1 << 7, "the last usage is the last bit");
        assert!(keys.is_down(0x04) && keys.is_down(0xE1));
        assert!(keys.set(0x04, false));
        assert!(!keys.is_down(0x04));
        assert!(keys.any());
    }

    #[test]
    fn the_power_key_and_the_error_codes_are_never_set() {
        let mut keys = Keys::new();
        for usage in [0x00, 0x01, 0x02, 0x03, POWER, 0xE8, 0xFF] {
            assert!(!keys.set(usage, true), "usage {usage:#x} set");
            assert!(!keys.is_down(usage));
        }
        assert_eq!(keys, Keys::new(), "nothing changed");
        assert!(keys_allowed(keys.report()));
        let mut forged = *keys.report();
        forged[BITMAP_AT + usize::from(POWER / 8)] |= 1 << (POWER % 8);
        assert!(!keys_allowed(&forged), "a report with the power key set");
    }

    #[test]
    fn a_consumer_usage_is_one_word_and_none_above_the_range() {
        assert_eq!(
            consumer_report(0xE2),
            Some([CONSUMER_REPORT_ID, 0xE2, 0x00])
        );
        assert_eq!(
            consumer_report(0x224),
            Some([CONSUMER_REPORT_ID, 0x24, 0x02])
        );
        assert_eq!(consumer_report(LAST_CONSUMER + 1), None);
        assert!(consumer_allowed(&consumer_report(LAST_CONSUMER).unwrap()));
        assert!(!consumer_allowed(&[CONSUMER_REPORT_ID, 0x00, 0x04]));
        assert_eq!(consumer_report(0), Some(NEUTRAL_CONSUMER));
    }

    /// **The descriptor declares what the encoder writes**: the modifiers'
    /// eight bits first, then 224 bits from usage 0x00, then one 16-bit
    /// consumer usage in a report of its own; no output report, so no lock
    /// lights, and no system control usage anywhere.
    #[test]
    fn the_descriptor_declares_the_reports_the_encoder_writes() {
        let fields = walk(DESCRIPTOR).unwrap();
        let keys: Vec<_> = fields
            .iter()
            .filter(|f| f.report == Report::Input && f.id == KEYS_REPORT_ID)
            .collect();
        assert_eq!(keys.len(), 2);
        assert_eq!(
            (keys[0].offset, keys[0].size, keys[0].count),
            (0, 1, 8),
            "the modifiers"
        );
        assert_eq!(
            (keys[0].usage_min, keys[0].usage_max),
            (Some(0x7_00E0), Some(0x7_00E7))
        );
        assert_eq!((keys[1].offset, keys[1].size, keys[1].count), (8, 1, 224));
        assert_eq!(
            (keys[1].usage_min, keys[1].usage_max),
            (Some(0x7_0000), Some(0x7_00DF))
        );
        assert!(keys.iter().all(|f| f.variable() && !f.constant()));
        assert_eq!(
            crate::walk::report_len(&fields, Report::Input, KEYS_REPORT_ID),
            KEYS_REPORT_LEN
        );
        assert_eq!(
            crate::walk::report_len(&fields, Report::Input, CONSUMER_REPORT_ID),
            CONSUMER_REPORT_LEN
        );
        let consumer = fields.iter().find(|f| f.id == CONSUMER_REPORT_ID).unwrap();
        assert!(!consumer.variable(), "an array of one usage");
        assert_eq!(
            (consumer.logical_min, consumer.logical_max),
            (0, i32::from(LAST_CONSUMER))
        );
        assert!(
            fields.iter().all(|f| f.report == Report::Input),
            "no output or feature report"
        );
        assert!(
            fields.iter().all(|f| !f.names(0x1_0080)),
            "no system control collection"
        );
    }
}
