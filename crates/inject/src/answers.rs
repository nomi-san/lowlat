//! What a pad sent as its reports answers a driver's first questions with,
//! whichever platform presents it (docs/05-host.md section 7.2): the peer's
//! own calibration and firmware, a real pad's for what it did not send, and a
//! pairing report around an address this host makes up. Linux answers each
//! question as the kernel asks it; Windows hands the driver the answers as the
//! pad is made.

use lowlat_core::pad::{self, Feature, Product};

/// The feature reports a device answers with: what the peer sent, and a
/// default for what it did not.
///
/// **The pairing report is never the peer's.** The address in it is the
/// device's identity to the driver, which refuses a second device with an
/// address it already has, and the same physical pad may be plugged into
/// this host beside its own passthrough. So the address is this host's,
/// unique per guest and pad, and the pairing answer is built around it.
#[derive(Debug, Clone, Copy)]
pub struct Features {
    calibration: Option<([u8; pad::FEATURE_MAX], usize)>,
    firmware: Option<([u8; pad::FEATURE_MAX], usize)>,
}

impl Default for Features {
    fn default() -> Self {
        Self::new()
    }
}

impl Features {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            calibration: None,
            firmware: None,
        }
    }

    /// Keep what the peer sent. A report longer than the largest is refused.
    pub fn set(&mut self, feature: Feature, report: &[u8]) {
        let mut kept = [0u8; pad::FEATURE_MAX];
        let Some(dst) = kept.get_mut(..report.len()) else {
            return;
        };
        dst.copy_from_slice(report);
        let slot = match feature {
            Feature::Calibration => &mut self.calibration,
            Feature::Firmware => &mut self.firmware,
        };
        *slot = Some((kept, report.len()));
    }

    /// The answer to the driver's question for one report identifier, or
    /// `None` for one nobody defined.
    pub(crate) fn answer(
        &self,
        product: Product,
        rnum: u8,
        address: &[u8; 6],
        out: &mut [u8; pad::FEATURE_MAX],
    ) -> Option<usize> {
        for (feature, kept, canned) in [
            (
                Feature::Calibration,
                self.calibration,
                canned(product, Feature::Calibration),
            ),
            (
                Feature::Firmware,
                self.firmware,
                canned(product, Feature::Firmware),
            ),
        ] {
            if feature.id(product) != rnum {
                continue;
            }
            let (bytes, len): (&[u8], usize) = match kept.as_ref() {
                Some((kept, len)) => (kept.get(..*len)?, *len),
                None => (canned, canned.len()),
            };
            out.get_mut(..len)?.copy_from_slice(bytes);
            return Some(len);
        }
        if rnum == pairing_id(product) {
            let template = pairing_template(product);
            let dst = out.get_mut(..template.len())?;
            dst.copy_from_slice(template);
            // The device's address, least significant byte first, where the
            // real report carries it.
            if let Some(slot) = dst.get_mut(1..7) {
                for (i, byte) in slot.iter_mut().enumerate() {
                    *byte = address.get(5 - i).copied().unwrap_or(0);
                }
            }
            return Some(template.len());
        }
        None
    }
}

/// The answers a real pad gave, for a peer that sent none.
pub(crate) fn canned(product: Product, feature: Feature) -> &'static [u8] {
    match (product, feature) {
        (Product::DualShock4, Feature::Calibration) => {
            include_bytes!("../../core/tests/data/pad/ds4/feature-calibration.bin")
        }
        (Product::DualShock4, Feature::Firmware) => {
            include_bytes!("../../core/tests/data/pad/ds4/feature-firmware.bin")
        }
        (Product::DualSense, Feature::Calibration) => {
            include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin")
        }
        (Product::DualSense, Feature::Firmware) => {
            include_bytes!("../../core/tests/data/pad/ds5/feature-firmware.bin")
        }
        (_, Feature::Calibration) => {
            include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin")
        }
        (_, Feature::Firmware) => {
            include_bytes!("../../core/tests/data/pad/ds5/feature-firmware.bin")
        }
    }
}

/// The pairing report's identifier: what a driver asks a USB device for.
pub(crate) const fn pairing_id(product: Product) -> u8 {
    match product {
        Product::DualShock4 => 0x12,
        _ => 0x09,
    }
}

/// The pairing report with both addresses zeroed, as the real pads answer
/// it; the device's own address is written in at 1..7.
pub(crate) fn pairing_template(product: Product) -> &'static [u8] {
    match product {
        Product::DualShock4 => include_bytes!("../../core/tests/data/pad/ds4/feature-pairing.bin"),
        _ => include_bytes!("../../core/tests/data/pad/ds5/feature-pairing.bin"),
    }
}

/// The middle of every pad address a guest's devices carry: the guest's own,
/// from its label, so two guests' pads never share one.
pub(crate) fn middle(guest: &str) -> [u8; 3] {
    let crc = lowlat_core::crc32::of(guest.as_bytes()).to_le_bytes();
    [crc[0], crc[1], crc[2]]
}

/// A pad's address: locally administered, the guest's middle, the pad's
/// slot among the guest's last -- unique on this host, which a driver
/// insists on.
pub(crate) fn address(middle: [u8; 3], index: usize) -> [u8; 6] {
    [
        0x02,
        middle[0],
        middle[1],
        middle[2],
        0x4c,
        u8::try_from(index).unwrap_or(u8::MAX),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The driver's three questions are answered with the peer's report
    /// where it sent one, the real pad's otherwise, and the pairing report
    /// carries this host's address least significant byte first where the
    /// real report has the pad's.
    #[test]
    fn the_questions_are_answered_from_the_peer_then_the_defaults() {
        let mut features = Features::new();
        let mut out = [0u8; pad::FEATURE_MAX];
        let address = [0x02, 0x4c, 0x4c, 0x01, 0x02, 0x03];
        // Defaults: the real DualSense's answers.
        assert_eq!(
            features.answer(Product::DualSense, 0x05, &address, &mut out),
            Some(41)
        );
        assert_eq!(&out[..41], canned(Product::DualSense, Feature::Calibration));
        assert_eq!(
            features.answer(Product::DualSense, 0x20, &address, &mut out),
            Some(64)
        );
        assert_eq!(&out[..64], canned(Product::DualSense, Feature::Firmware));
        // Pairing: the template with the address reversed at 1..7.
        assert_eq!(
            features.answer(Product::DualSense, 0x09, &address, &mut out),
            Some(20)
        );
        assert_eq!(out[0], 0x09);
        assert_eq!(&out[1..7], &[0x03, 0x02, 0x01, 0x4c, 0x4c, 0x02]);
        assert_eq!(&out[7..10], &[0x08, 0x25, 0x00]);
        assert_eq!(
            features.answer(Product::DualShock4, 0x12, &address, &mut out),
            Some(16)
        );
        assert_eq!(out[0], 0x12);
        assert_eq!(&out[1..7], &[0x03, 0x02, 0x01, 0x4c, 0x4c, 0x02]);
        // A question nobody defined.
        assert_eq!(
            features.answer(Product::DualSense, 0x81, &address, &mut out),
            None
        );
        assert_eq!(
            features.answer(Product::DualShock4, 0x09, &address, &mut out),
            None
        );
        // The peer's own, once sent.
        let mut mine = [0u8; 37];
        mine[0] = 0x02;
        mine[1] = 0xEE;
        features.set(Feature::Calibration, &mine);
        assert_eq!(
            features.answer(Product::DualShock4, 0x02, &address, &mut out),
            Some(37)
        );
        assert_eq!(&out[..37], &mine);
        assert_eq!(
            features.answer(Product::DualShock4, 0xA3, &address, &mut out),
            Some(49)
        );
        assert_eq!(&out[..49], canned(Product::DualShock4, Feature::Firmware));
    }

    /// The canned answers are the sizes the driver insists on.
    #[test]
    fn the_defaults_are_the_sizes_the_driver_reads() {
        for (product, feature) in [
            (Product::DualShock4, Feature::Calibration),
            (Product::DualShock4, Feature::Firmware),
            (Product::DualSense, Feature::Calibration),
            (Product::DualSense, Feature::Firmware),
        ] {
            let bytes = canned(product, feature);
            assert_eq!(bytes.len(), feature.len(product), "{product:?} {feature:?}");
            assert_eq!(bytes[0], feature.id(product));
        }
        assert_eq!(pairing_template(Product::DualShock4).len(), 16);
        assert_eq!(pairing_template(Product::DualSense).len(), 20);
    }

    /// **A pad's address is its guest's and its slot's**: locally
    /// administered, the guest's middle from its label, the slot last, so two
    /// guests' pads differ and a guest's own pads differ.
    #[test]
    fn a_pads_address_is_its_guests_and_its_slots() {
        let one = middle("7");
        let crc = lowlat_core::crc32::of(b"7").to_le_bytes();
        assert_eq!(one, [crc[0], crc[1], crc[2]]);
        assert_ne!(one, middle("8"));
        assert_eq!(address(one, 2), [0x02, one[0], one[1], one[2], 0x4c, 2]);
        assert_ne!(address(one, 0), address(one, 1));
    }
}
