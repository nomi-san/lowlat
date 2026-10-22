//! The mouse: a relative collection and an absolute one (docs/11-drivers.md
//! section 2.3). **Every button and wheel rides the relative collection, and
//! the absolute one carries positions alone**, its buttons declared and never
//! pressed: each collection is a pointer of its own to the system, with its
//! own buttons, so a release sent on the other would leave a button held, and
//! an absolute report carries a position, so a button sent on it after
//! relative motion would throw the pointer back to where it last was.
//!
//! The wheel and the horizontal wheel each sit under a resolution multiplier
//! the system sets, so a count is a hundred and twentieth of a notch: the
//! units a peer's wheel arrives in pass whole.

/// Buttons, motion, the wheel and the horizontal wheel.
pub const RELATIVE_REPORT_ID: u8 = 1;
pub const RELATIVE_REPORT_LEN: usize = 10;
/// A position.
pub const ABSOLUTE_REPORT_ID: u8 = 2;
pub const ABSOLUTE_REPORT_LEN: usize = 6;
/// The two wheels' resolution multipliers, a feature report the system sets.
pub const MULTIPLIER_REPORT_ID: u8 = 1;
pub const MULTIPLIER_REPORT_LEN: usize = 2;
/// A wheel count is this part of a notch once the system sets the multiplier.
pub const MULTIPLIER: u8 = 120;

/// Each relative axis and wheel, either side of zero.
pub const RELATIVE_MAX: i16 = 32767;
/// The absolute collection's range on both axes.
pub const ABSOLUTE_MAX: u16 = 32767;

pub const LEFT: u8 = 1 << 0;
pub const RIGHT: u8 = 1 << 1;
pub const MIDDLE: u8 = 1 << 2;
pub const BACK: u8 = 1 << 3;
pub const FORWARD: u8 = 1 << 4;
/// The five buttons the relative collection declares.
pub const BUTTONS: u8 = 0x1F;

pub const DESCRIPTOR: &[u8] = &[
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x01, //   Report ID (1)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x05, //     Usage Maximum (5)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x75, 0x01, //     Report Size (1)
    0x95, 0x05, //     Report Count (5)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x75, 0x03, //     Report Size (3)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x03, //     Input (Constant)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x16, 0x01, 0x80, //     Logical Minimum (-32767)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x06, //     Input (Data, Variable, Relative)
    0xA1, 0x02, //     Collection (Logical)
    0x09, 0x48, //       Usage (Resolution Multiplier)
    0x15, 0x00, //       Logical Minimum (0)
    0x25, 0x01, //       Logical Maximum (1)
    0x35, 0x01, //       Physical Minimum (1)
    0x45, 0x78, //       Physical Maximum (120)
    0x75, 0x02, //       Report Size (2)
    0x95, 0x01, //       Report Count (1)
    0xB1, 0x02, //       Feature (Data, Variable, Absolute)
    0x09, 0x38, //       Usage (Wheel)
    0x16, 0x01, 0x80, //       Logical Minimum (-32767)
    0x26, 0xFF, 0x7F, //       Logical Maximum (32767)
    0x35, 0x00, //       Physical Minimum (0)
    0x45, 0x00, //       Physical Maximum (0)
    0x75, 0x10, //       Report Size (16)
    0x81, 0x06, //       Input (Data, Variable, Relative)
    0xC0, //     End Collection
    0xA1, 0x02, //     Collection (Logical)
    0x09, 0x48, //       Usage (Resolution Multiplier)
    0x15, 0x00, //       Logical Minimum (0)
    0x25, 0x01, //       Logical Maximum (1)
    0x35, 0x01, //       Physical Minimum (1)
    0x45, 0x78, //       Physical Maximum (120)
    0x75, 0x02, //       Report Size (2)
    0xB1, 0x02, //       Feature (Data, Variable, Absolute)
    0x35, 0x00, //       Physical Minimum (0)
    0x45, 0x00, //       Physical Maximum (0)
    0x75, 0x04, //       Report Size (4)
    0xB1, 0x03, //       Feature (Constant)
    0x05, 0x0C, //       Usage Page (Consumer)
    0x0A, 0x38, 0x02, //       Usage (AC Pan)
    0x16, 0x01, 0x80, //       Logical Minimum (-32767)
    0x26, 0xFF, 0x7F, //       Logical Maximum (32767)
    0x75, 0x10, //       Report Size (16)
    0x81, 0x06, //       Input (Data, Variable, Relative)
    0xC0, //     End Collection
    0xC0, //   End Collection
    0xC0, // End Collection
    0x05, 0x01, // Usage Page (Generic Desktop)
    0x09, 0x02, // Usage (Mouse)
    0xA1, 0x01, // Collection (Application)
    0x85, 0x02, //   Report ID (2)
    0x09, 0x01, //   Usage (Pointer)
    0xA1, 0x00, //   Collection (Physical)
    0x05, 0x09, //     Usage Page (Button)
    0x19, 0x01, //     Usage Minimum (1)
    0x29, 0x05, //     Usage Maximum (5)
    0x15, 0x00, //     Logical Minimum (0)
    0x25, 0x01, //     Logical Maximum (1)
    0x75, 0x01, //     Report Size (1)
    0x95, 0x05, //     Report Count (5)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0x75, 0x03, //     Report Size (3)
    0x95, 0x01, //     Report Count (1)
    0x81, 0x03, //     Input (Constant)
    0x05, 0x01, //     Usage Page (Generic Desktop)
    0x09, 0x30, //     Usage (X)
    0x09, 0x31, //     Usage (Y)
    0x15, 0x00, //     Logical Minimum (0)
    0x26, 0xFF, 0x7F, //     Logical Maximum (32767)
    0x75, 0x10, //     Report Size (16)
    0x95, 0x02, //     Report Count (2)
    0x81, 0x02, //     Input (Data, Variable, Absolute)
    0xC0, //   End Collection
    0xC0, // End Collection
];

/// No button, no motion, no wheel: what the mouse's end submits, which moves
/// nothing.
pub const NEUTRAL: [u8; RELATIVE_REPORT_LEN] = relative(0, 0, 0, 0, 0);

/// The multipliers before the system sets them.
pub const MULTIPLIER_DEFAULT: [u8; MULTIPLIER_REPORT_LEN] = [MULTIPLIER_REPORT_ID, 0];

/// One relative axis, the descriptor's range being one short of the word's.
const fn axis(value: i16) -> [u8; 2] {
    let value = if value < -RELATIVE_MAX {
        -RELATIVE_MAX
    } else {
        value
    };
    value.to_le_bytes()
}

/// The relative report: `buttons` from [`BUTTONS`], the motion, and the two
/// wheels in hundred-and-twentieths of a notch. A value past the range is
/// held at its end; motion larger than a report carries is the caller's to
/// split.
pub const fn relative(
    buttons: u8,
    dx: i16,
    dy: i16,
    wheel: i16,
    pan: i16,
) -> [u8; RELATIVE_REPORT_LEN] {
    let [x0, x1] = axis(dx);
    let [y0, y1] = axis(dy);
    let [w0, w1] = axis(wheel);
    let [p0, p1] = axis(pan);
    [
        RELATIVE_REPORT_ID,
        buttons & BUTTONS,
        x0,
        x1,
        y0,
        y1,
        w0,
        w1,
        p0,
        p1,
    ]
}

/// The absolute report: a position, never a button.
pub const fn absolute(x: u16, y: u16) -> [u8; ABSOLUTE_REPORT_LEN] {
    let x = if x > ABSOLUTE_MAX { ABSOLUTE_MAX } else { x };
    let y = if y > ABSOLUTE_MAX { ABSOLUTE_MAX } else { y };
    let [x0, x1] = x.to_le_bytes();
    let [y0, y1] = y.to_le_bytes();
    [ABSOLUTE_REPORT_ID, 0, x0, x1, y0, y1]
}

/// Whether an absolute report may reach the system: no button pressed, and
/// the position within the range.
pub fn absolute_allowed(report: &[u8]) -> bool {
    match report {
        [_, buttons, x0, x1, y0, y1] => {
            *buttons == 0
                && u16::from_le_bytes([*x0, *x1]) <= ABSOLUTE_MAX
                && u16::from_le_bytes([*y0, *y1]) <= ABSOLUTE_MAX
        }
        _ => false,
    }
}

/// Whether a relative report may reach the system: no button the collection
/// does not declare, and no value one past the range.
pub fn relative_allowed(report: &[u8]) -> bool {
    match report {
        [_, buttons, rest @ ..] if rest.len() == 8 => *buttons & !BUTTONS == 0
            && rest.chunks_exact(2).all(
                |word| matches!(word, [low, high] if i16::from_le_bytes([*low, *high]) != i16::MIN),
            ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{Report, report_len, walk};
    use std::vec::Vec;

    #[test]
    fn a_relative_report_carries_buttons_motion_and_both_wheels() {
        let report = relative(LEFT | FORWARD, -3, 300, 120, -1);
        assert_eq!(
            report,
            [1, 0b1_0001, 0xFD, 0xFF, 0x2C, 0x01, 0x78, 0x00, 0xFF, 0xFF]
        );
        assert!(relative_allowed(&report));
        assert_eq!(
            relative(0xFF, 0, 0, 0, 0)[1],
            BUTTONS,
            "undeclared buttons dropped"
        );
        assert_eq!(
            relative(0, i16::MIN, i16::MAX, 0, 0)[2..6],
            [0x01, 0x80, 0xFF, 0x7F],
            "held at the range's ends"
        );
        let mut forged = report;
        forged[2..4].copy_from_slice(&i16::MIN.to_le_bytes());
        assert!(!relative_allowed(&forged));
        forged = report;
        forged[1] = 0x20;
        assert!(!relative_allowed(&forged), "a sixth button");
    }

    #[test]
    fn an_absolute_report_is_a_position_and_never_a_button() {
        assert_eq!(absolute(1, 0x7FFF), [2, 0, 1, 0, 0xFF, 0x7F]);
        assert_eq!(absolute(u16::MAX, 0)[2..4], ABSOLUTE_MAX.to_le_bytes());
        assert!(absolute_allowed(&absolute(16383, 16383)));
        let mut pressed = absolute(0, 0);
        pressed[1] = LEFT;
        assert!(
            !absolute_allowed(&pressed),
            "a button on the absolute collection"
        );
        assert!(!absolute_allowed(&[2, 0, 0, 0x80, 0, 0]), "past the range");
    }

    /// **The descriptor declares what the encoders write**, and the
    /// multipliers stand in a feature report of their own over the two wheels.
    #[test]
    fn the_descriptor_declares_the_reports_the_encoders_write() {
        let fields = walk(DESCRIPTOR).unwrap();
        assert_eq!(
            report_len(&fields, Report::Input, RELATIVE_REPORT_ID),
            RELATIVE_REPORT_LEN
        );
        assert_eq!(
            report_len(&fields, Report::Input, ABSOLUTE_REPORT_ID),
            ABSOLUTE_REPORT_LEN
        );
        assert_eq!(
            report_len(&fields, Report::Feature, MULTIPLIER_REPORT_ID),
            MULTIPLIER_REPORT_LEN
        );
        let relative: Vec<_> = fields
            .iter()
            .filter(|f| f.report == Report::Input && f.id == RELATIVE_REPORT_ID && !f.constant())
            .collect();
        let named = |usage: u32| relative.iter().find(|f| f.names(usage)).unwrap();
        assert_eq!(
            (named(0x9_0001).offset, named(0x9_0001).count),
            (0, 5),
            "buttons"
        );
        for (usage, offset) in [
            (0x1_0030, 8),
            (0x1_0031, 24),
            (0x1_0038, 40),
            (0xC_0238, 56),
        ] {
            let field = named(usage);
            assert!(field.relative(), "usage {usage:#x} relative");
            assert_eq!(
                field.offset + field.size * field.element(usage).unwrap(),
                offset,
                "usage {usage:#x}"
            );
            assert_eq!((field.logical_min, field.logical_max), (-32767, 32767));
        }
        let multipliers: Vec<_> = fields
            .iter()
            .filter(|f| f.report == Report::Feature && f.names(0x1_0048))
            .collect();
        assert_eq!(multipliers.len(), 2, "one for each wheel");
        assert!(
            multipliers
                .iter()
                .all(|f| (f.physical_min, f.physical_max) == (1, i32::from(MULTIPLIER)))
        );
        let absolute: Vec<_> = fields
            .iter()
            .filter(|f| f.report == Report::Input && f.id == ABSOLUTE_REPORT_ID && !f.constant())
            .collect();
        let position = absolute.iter().find(|f| f.names(0x1_0030)).unwrap();
        assert!(!position.relative());
        assert_eq!(
            (position.logical_min, position.logical_max),
            (0, i32::from(ABSOLUTE_MAX))
        );
        assert!(
            absolute.iter().any(|f| f.names(0x9_0001)),
            "its buttons are declared"
        );
    }
}
