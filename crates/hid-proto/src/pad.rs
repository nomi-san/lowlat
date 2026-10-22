//! The DualShock 4 and the DualSense, as the real products present themselves
//! on USB: the descriptors, the reports at rest and the answers to the
//! system's first questions are the ones the Linux host presents too
//! (docs/07-platforms.md section 4.2).

/// The input report both pads send, identifier byte first.
pub const INPUT_REPORT_ID: u8 = 1;
pub const INPUT_REPORT_LEN: usize = 64;

pub const DS4_DESCRIPTOR: &[u8] = include_bytes!("../../core/tests/data/pad/ds4/descriptor.bin");
pub const DS5_DESCRIPTOR: &[u8] = include_bytes!("../../core/tests/data/pad/ds5/descriptor.bin");

/// A pad at rest, as a real one sends it: no button, the d-pad released and
/// the sticks centred. The d-pad is a hat whose released value lies outside
/// its range, so a report of zeroes would hold it up and pull both sticks into
/// a corner.
pub const DS4_NEUTRAL: &[u8; INPUT_REPORT_LEN] =
    include_bytes!("../../core/tests/data/pad/ds4/input-idle.bin");
pub const DS5_NEUTRAL: &[u8; INPUT_REPORT_LEN] =
    include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");

/// What a real pad answers before anybody sets it: its calibration, its
/// firmware and its pairing, each identifier byte first.
pub const DS4_FEATURES: [&[u8]; 3] = [
    include_bytes!("../../core/tests/data/pad/ds4/feature-calibration.bin"),
    include_bytes!("../../core/tests/data/pad/ds4/feature-firmware.bin"),
    include_bytes!("../../core/tests/data/pad/ds4/feature-pairing.bin"),
];
pub const DS5_FEATURES: [&[u8]; 3] = [
    include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin"),
    include_bytes!("../../core/tests/data/pad/ds5/feature-firmware.bin"),
    include_bytes!("../../core/tests/data/pad/ds5/feature-pairing.bin"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::walk::{Report, report_len, walk};

    /// **A pad at rest holds nothing**, read through the pad's own
    /// descriptor: every button up, the hat at a value outside its range --
    /// released -- and each stick within one of its range's middle.
    #[test]
    fn a_pad_at_rest_holds_nothing_by_its_own_descriptor() {
        for (name, descriptor, neutral) in [
            ("DualShock 4", DS4_DESCRIPTOR, DS4_NEUTRAL),
            ("DualSense", DS5_DESCRIPTOR, DS5_NEUTRAL),
        ] {
            let fields = walk(descriptor).unwrap();
            assert_eq!(
                report_len(&fields, Report::Input, INPUT_REPORT_ID),
                INPUT_REPORT_LEN,
                "{name}"
            );
            assert_eq!(neutral[0], INPUT_REPORT_ID, "{name}");
            let input = || {
                fields.iter().filter(|f| {
                    f.report == Report::Input && f.id == INPUT_REPORT_ID && !f.constant()
                })
            };
            let hat = input().find(|f| f.names(0x1_0039)).unwrap();
            assert!(hat.null_state(), "{name}: the hat has a released value");
            let at = hat.element(0x1_0039).unwrap();
            let value = i64::from(hat.value(neutral, at));
            assert!(
                value < i64::from(hat.logical_min) || value > i64::from(hat.logical_max),
                "{name}: the hat at rest reads {value}, inside its range"
            );
            for stick in [0x1_0030, 0x1_0031, 0x1_0032, 0x1_0035] {
                let field = input().find(|f| f.names(stick)).unwrap();
                let value = i64::from(field.value(neutral, field.element(stick).unwrap()));
                let middle = (i64::from(field.logical_min) + i64::from(field.logical_max)) / 2;
                assert!(
                    (value - middle).abs() <= 1,
                    "{name}: stick {stick:#x} at {value}"
                );
            }
            for buttons in input().filter(|f| f.page() == 0x9) {
                for element in 0..buttons.count {
                    assert_eq!(
                        buttons.value(neutral, element),
                        0,
                        "{name}: a button down at rest"
                    );
                }
            }
        }
    }

    #[test]
    fn the_answers_are_whole_reports_the_descriptor_declares() {
        for (name, descriptor, features) in [
            ("DualShock 4", DS4_DESCRIPTOR, DS4_FEATURES),
            ("DualSense", DS5_DESCRIPTOR, DS5_FEATURES),
        ] {
            let fields = walk(descriptor).unwrap();
            for report in features {
                let id = report[0];
                assert_eq!(
                    report_len(&fields, Report::Feature, id),
                    report.len(),
                    "{name}: feature {id:#x}"
                );
                assert!(
                    report.len() <= crate::control::FEATURE_MAX,
                    "{name}: feature {id:#x}"
                );
            }
        }
    }
}
