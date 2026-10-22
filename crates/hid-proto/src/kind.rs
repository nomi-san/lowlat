//! The four kinds of device the driver makes. **A caller names a kind and
//! never describes a device**: each kind's identity, descriptor and reports
//! are compiled into the driver and the host alike, so nothing a caller sends
//! is a descriptor (docs/11-drivers.md section 2.2).

use crate::{GUEST_SLOTS, PADS_PER_GUEST, keyboard, mouse, pad};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    Keyboard = 1,
    Mouse = 2,
    DualShock4 = 3,
    DualSense = 4,
}

/// What the system knows a device by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// The identity a keyboard and a mouse carry: the open-source projects'
/// shared vendor and its test products, never a real vendor's.
const OPEN_SOURCE: u16 = 0x1209;
/// The pads carry the real products' own, which is what they are recognised
/// by.
const SONY: u16 = 0x054c;
/// What the real pads report as their version.
const PAD_VERSION: u16 = 0x8111;

impl Kind {
    pub const ALL: [Kind; 4] = [
        Kind::Keyboard,
        Kind::Mouse,
        Kind::DualShock4,
        Kind::DualSense,
    ];

    pub const fn from_u32(value: u32) -> Option<Kind> {
        match value {
            1 => Some(Kind::Keyboard),
            2 => Some(Kind::Mouse),
            3 => Some(Kind::DualShock4),
            4 => Some(Kind::DualSense),
            _ => None,
        }
    }

    pub const fn identity(self) -> Identity {
        match self {
            Kind::Keyboard => Identity {
                vendor: OPEN_SOURCE,
                product: 0x0001,
                version: 0x0100,
            },
            Kind::Mouse => Identity {
                vendor: OPEN_SOURCE,
                product: 0x0002,
                version: 0x0100,
            },
            Kind::DualShock4 => Identity {
                vendor: SONY,
                product: 0x09cc,
                version: PAD_VERSION,
            },
            Kind::DualSense => Identity {
                vendor: SONY,
                product: 0x0ce6,
                version: PAD_VERSION,
            },
        }
    }

    pub const fn descriptor(self) -> &'static [u8] {
        match self {
            Kind::Keyboard => keyboard::DESCRIPTOR,
            Kind::Mouse => mouse::DESCRIPTOR,
            Kind::DualShock4 => pad::DS4_DESCRIPTOR,
            Kind::DualSense => pad::DS5_DESCRIPTOR,
        }
    }

    /// How many slots the kind has: one per seated guest for the keyboard and
    /// the mouse, one per guest's pad for the pads.
    pub const fn slots(self) -> u32 {
        match self {
            Kind::Keyboard | Kind::Mouse => GUEST_SLOTS,
            Kind::DualShock4 | Kind::DualSense => GUEST_SLOTS * PADS_PER_GUEST,
        }
    }

    /// The length of the input report with identifier `id`, the identifier
    /// included; none for a report the kind does not send.
    pub const fn input_len(self, id: u8) -> Option<usize> {
        match (self, id) {
            (Kind::Keyboard, keyboard::KEYS_REPORT_ID) => Some(keyboard::KEYS_REPORT_LEN),
            (Kind::Keyboard, keyboard::CONSUMER_REPORT_ID) => Some(keyboard::CONSUMER_REPORT_LEN),
            (Kind::Mouse, mouse::RELATIVE_REPORT_ID) => Some(mouse::RELATIVE_REPORT_LEN),
            (Kind::Mouse, mouse::ABSOLUTE_REPORT_ID) => Some(mouse::ABSOLUTE_REPORT_LEN),
            (Kind::DualShock4 | Kind::DualSense, pad::INPUT_REPORT_ID) => {
                Some(pad::INPUT_REPORT_LEN)
            }
            _ => None,
        }
    }

    /// Whether `report` is one the device takes: an input report the kind
    /// declares, whole, and nothing no guest may send -- the keyboard's power
    /// key, a button on the mouse's absolute collection. The driver checks
    /// every write against this before the system sees it.
    pub fn accepts(self, report: &[u8]) -> bool {
        let Some(&id) = report.first() else {
            return false;
        };
        if self.input_len(id) != Some(report.len()) {
            return false;
        }
        match (self, id) {
            (Kind::Keyboard, keyboard::KEYS_REPORT_ID) => keyboard::keys_allowed(report),
            (Kind::Keyboard, _) => keyboard::consumer_allowed(report),
            (Kind::Mouse, mouse::RELATIVE_REPORT_ID) => mouse::relative_allowed(report),
            (Kind::Mouse, _) => mouse::absolute_allowed(report),
            (Kind::DualShock4 | Kind::DualSense, _) => true,
        }
    }

    /// The reports that hold nothing down, which the driver submits before a
    /// device goes, so a host that ends leaves nothing held.
    pub const fn neutral(self) -> &'static [&'static [u8]] {
        match self {
            Kind::Keyboard => &[&keyboard::NEUTRAL_KEYS, &keyboard::NEUTRAL_CONSUMER],
            Kind::Mouse => &[&mouse::NEUTRAL],
            Kind::DualShock4 => &[pad::DS4_NEUTRAL],
            Kind::DualSense => &[pad::DS5_NEUTRAL],
        }
    }

    /// The feature reports the device answers with until a caller's table or
    /// the system's own write replaces one.
    pub const fn features(self) -> &'static [&'static [u8]] {
        match self {
            Kind::Keyboard => &[],
            Kind::Mouse => &[&mouse::MULTIPLIER_DEFAULT],
            Kind::DualShock4 => &pad::DS4_FEATURES,
            Kind::DualSense => &pad::DS5_FEATURES,
        }
    }

    /// The two letters a device's instance starts with.
    const fn prefix(self) -> [u8; 2] {
        match self {
            Kind::Keyboard => *b"kb",
            Kind::Mouse => *b"ms",
            Kind::DualShock4 => *b"d4",
            Kind::DualSense => *b"d5",
        }
    }

    /// The device's instance, the same for a kind and slot at every
    /// connection, so the system sets that device up once rather than at each.
    /// None for a slot the kind does not have.
    pub fn instance_id(self, slot: u8) -> Option<InstanceId> {
        if u32::from(slot) >= self.slots() {
            return None;
        }
        let [first, second] = self.prefix();
        Some(InstanceId {
            units: [
                u16::from(first),
                u16::from(second),
                u16::from(b'0' + slot / 10),
                u16::from(b'0' + slot % 10),
                0,
            ],
        })
    }
}

/// A device's instance: the kind's two letters and the slot in two digits, in
/// the system's wide characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceId {
    units: [u16; 5],
}

impl InstanceId {
    /// The characters, terminated.
    pub const fn with_nul(&self) -> &[u16; 5] {
        &self.units
    }

    /// The length in bytes, the terminator left out.
    pub const fn byte_len(&self) -> u16 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{Report, report_len, walk};
    use std::collections::BTreeSet;

    #[test]
    fn a_kind_is_its_number_and_nothing_else_is_one() {
        for kind in Kind::ALL {
            assert_eq!(Kind::from_u32(kind as u32), Some(kind));
        }
        assert_eq!(Kind::from_u32(0), None);
        assert_eq!(Kind::from_u32(5), None);
    }

    /// **Every input report a descriptor declares is one the kind takes, at
    /// its declared length**, and no other: a write the driver would pass
    /// that the system does not expect is refused, and none it expects is.
    #[test]
    fn each_kind_takes_exactly_the_input_reports_its_descriptor_declares() {
        for kind in Kind::ALL {
            let fields = walk(kind.descriptor()).unwrap();
            let declared: BTreeSet<u8> = fields
                .iter()
                .filter(|f| f.report == Report::Input)
                .map(|f| f.id)
                .collect();
            for id in 0..=u8::MAX {
                let len = kind.input_len(id);
                assert_eq!(
                    len.is_some(),
                    declared.contains(&id),
                    "{kind:?} report {id}"
                );
                if let Some(len) = len {
                    assert_eq!(
                        len,
                        report_len(&fields, Report::Input, id),
                        "{kind:?} report {id}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_report_is_taken_whole_and_only_as_declared() {
        let keys = keyboard::NEUTRAL_KEYS;
        assert!(Kind::Keyboard.accepts(&keys));
        assert!(!Kind::Keyboard.accepts(&keys[..29]), "cut short");
        assert!(!Kind::Mouse.accepts(&keys), "another kind's report");
        assert!(!Kind::Keyboard.accepts(&[]));
        assert!(Kind::Mouse.accepts(&mouse::absolute(10, 10)));
        let mut pressed = mouse::absolute(10, 10);
        pressed[1] = mouse::LEFT;
        assert!(!Kind::Mouse.accepts(&pressed));
        let mut power = *keyboard::Keys::new().report();
        power[2 + usize::from(keyboard::POWER / 8)] |= 1 << (keyboard::POWER % 8);
        assert!(!Kind::Keyboard.accepts(&power));
        assert!(Kind::DualSense.accepts(pad::DS5_NEUTRAL));
        assert!(!Kind::DualSense.accepts(&pad::DS5_NEUTRAL[..63]));
    }

    /// **The keyboard's and the mouse's neutral reports hold nothing**: every
    /// data field reads zero through their own descriptors -- keys up, no
    /// consumer usage, buttons up, no motion and no wheel. The pads' are
    /// checked beside their descriptors (`pad`).
    #[test]
    fn the_keyboard_and_mouse_neutral_reports_hold_nothing() {
        for kind in [Kind::Keyboard, Kind::Mouse] {
            let fields = walk(kind.descriptor()).unwrap();
            for report in kind.neutral() {
                assert!(kind.accepts(report), "{kind:?}");
                for field in fields
                    .iter()
                    .filter(|f| f.report == Report::Input && f.id == report[0] && !f.constant())
                {
                    for element in 0..field.count {
                        assert_eq!(field.value(report, element), 0, "{kind:?} {field:?}");
                    }
                }
            }
        }
        for kind in [Kind::DualShock4, Kind::DualSense] {
            assert!(
                kind.neutral().iter().all(|report| kind.accepts(report)),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn an_instance_is_the_kinds_letters_and_its_slot() {
        let id = Kind::DualSense.instance_id(63).unwrap();
        let text: std::string::String = id.with_nul()[..4]
            .iter()
            .map(|unit| char::from(u8::try_from(*unit).unwrap()))
            .collect();
        assert_eq!(text, "d563");
        assert_eq!(id.with_nul()[4], 0);
        assert_eq!(
            Kind::Keyboard.instance_id(7).unwrap().with_nul()[..4],
            [107, 98, 48, 55]
        );
        assert_eq!(
            Kind::Keyboard.instance_id(16),
            None,
            "a keyboard has a guest's slots"
        );
        assert!(Kind::DualShock4.instance_id(63).is_some());
        assert_eq!(Kind::DualShock4.instance_id(64), None);
    }

    #[test]
    fn the_keyboard_and_mouse_carry_no_real_vendors_identity() {
        for kind in [Kind::Keyboard, Kind::Mouse] {
            assert_eq!(kind.identity().vendor, OPEN_SOURCE, "{kind:?}");
        }
        assert_ne!(Kind::Keyboard.identity(), Kind::Mouse.identity());
        assert_eq!(Kind::DualShock4.identity().product, 0x09cc);
        assert_eq!(Kind::DualSense.identity().product, 0x0ce6);
    }
}
