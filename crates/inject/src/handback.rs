//! What a guest's devices hand back, whatever the platform builds them on:
//! what an application wrote to a pad, a pad's reports on their way to an
//! application that holds the device, and what a pad was asked to vibrate at.

use crate::gamepad::MAX_PADS;
use lowlat_core::pad::{self, INPUT_LEN, Inbound, Product};

/// The largest report the device is written: the DualSense's output report
/// is 63 bytes, the feature writes at most 64.
pub const WRITTEN_MAX: usize = pad::FEATURE_MAX;

/// What an application on this host wrote to the device, for the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Written {
    pub kind: pad::OutputKind,
    pub len: usize,
    /// The report, identifier byte first: the USB form.
    pub report: [u8; WRITTEN_MAX],
}

/// What a pad is asked to vibrate at, on the way back to its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rumble {
    /// The peer's own identifier for the pad, so it needs no mapping back.
    pub pad: u32,
    /// **Eight bits each, as the peer carries them.** The kernel counts a
    /// motor in sixteen and the peer widens what it is sent by repeating the
    /// byte, so taking the high byte is exactly the inverse and a value
    /// survives the round trip unchanged.
    pub large: u8,
    pub small: u8,
}

/// What an application wrote to one of a guest's HID pads, for the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadWritten {
    pub pad: u32,
    pub written: Written,
}

/// A report pad's report on its way to the application rather than into a
/// device of this host's own, and the end of such a pad (docs/05-host.md
/// section 7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forwarded {
    /// An input report, the whole USB form.
    Input {
        pad: u32,
        product: Product,
        report: [u8; INPUT_LEN],
    },
    /// A feature report: `len` bytes, identifier byte first.
    Feature {
        pad: u32,
        product: Product,
        len: usize,
        report: [u8; INPUT_LEN],
    },
    /// The pad is gone: unplugged by its peer, or its guest left. Told after
    /// the pad's last report, so a device destroyed on it has seen them all.
    Unplugged { pad: u32, product: Product },
}

/// Where forwarded reports go. A call rather than a queue: the application's
/// queue is across a boundary this crate does not see, and the report is in
/// it one hop after it was parsed rather than two.
pub struct Forward(pub Box<dyn FnMut(Forwarded) + Send>);

impl core::fmt::Debug for Forward {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Forward")
    }
}

/// The report pads of a guest whose application holds the devices.
///
/// **The same rules as a device of this host's own, without the device.** The
/// product is fixed by the pad's first report; nothing is kept, because the
/// application's device is what needs the feature reports and it is given
/// them in the order they came; and the pad's end is told, on an unplug and
/// on the guest's, which is what lets the application destroy its device on
/// the same rule the host destroys its own.
#[derive(Debug)]
pub(crate) struct Forwarding {
    to: Forward,
    pads: [Option<(u32, Product)>; MAX_PADS],
}

impl Forwarding {
    pub(crate) fn new(to: Forward) -> Self {
        Self {
            to,
            pads: [const { None }; MAX_PADS],
        }
    }

    pub(crate) fn report(&mut self, pad: u32, inbound: &Inbound<'_>) {
        let (product, forwarded) = match *inbound {
            Inbound::Input { product, report } => (
                product,
                Forwarded::Input {
                    pad,
                    product,
                    report,
                },
            ),
            Inbound::Feature {
                product, report, ..
            } => {
                let mut bytes = [0u8; INPUT_LEN];
                let len = report.len().min(bytes.len());
                if let (Some(target), Some(source)) = (bytes.get_mut(..len), report.get(..len)) {
                    target.copy_from_slice(source);
                }
                (
                    product,
                    Forwarded::Feature {
                        pad,
                        product,
                        len,
                        report: bytes,
                    },
                )
            }
            Inbound::TouchBlock => return,
        };
        match self.pads.iter().flatten().find(|(id, _)| *id == pad) {
            // Fixed by the first report; one naming another product is not
            // this pad's.
            Some((_, fixed)) if *fixed != product => return,
            Some(_) => {}
            None => {
                // The expander's cap is in front of this, so there is room.
                let Some(slot) = self.pads.iter_mut().find(|slot| slot.is_none()) else {
                    return;
                };
                *slot = Some((pad, product));
            }
        }
        (self.to.0)(forwarded);
    }

    /// The product a forwarded pad's first report fixed.
    #[cfg(windows)]
    pub(crate) fn product(&self, pad: u32) -> Option<Product> {
        self.pads
            .iter()
            .flatten()
            .find(|(id, _)| *id == pad)
            .map(|(_, product)| *product)
    }

    /// Whether the pad was one forwarded, and so one the application was
    /// told about.
    pub(crate) fn unplug(&mut self, pad: u32) -> bool {
        let Some((_, product)) = self
            .pads
            .iter_mut()
            .find(|slot| slot.is_some_and(|(id, _)| id == pad))
            .and_then(Option::take)
        else {
            return false;
        };
        (self.to.0)(Forwarded::Unplugged { pad, product });
        true
    }
}

impl Drop for Forwarding {
    /// **A guest's end ends its pads.** The devices of this host's own go
    /// with their descriptors; the application is told, pad by pad.
    fn drop(&mut self) {
        for (pad, product) in self.pads.iter_mut().filter_map(Option::take) {
            (self.to.0)(Forwarded::Unplugged { pad, product });
        }
    }
}

#[cfg(test)]
mod forwarding_tests {
    use super::*;
    use lowlat_core::pad::Feature;
    use std::sync::{Arc, Mutex};

    /// A forwarding whose every call is written down.
    fn forwarding() -> (Forwarding, Arc<Mutex<Vec<Forwarded>>>) {
        let told = Arc::new(Mutex::new(Vec::new()));
        let into = Arc::clone(&told);
        let forwarding = Forwarding::new(Forward(Box::new(move |forwarded| {
            into.lock().unwrap().push(forwarded);
        })));
        (forwarding, told)
    }

    const DS5: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
    const DS4: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds4/input-idle.bin");
    const CALIBRATION: &[u8] =
        include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin");

    /// **The reports reach the application in the order they came**, the
    /// feature report whole with its length, the input report whole; the
    /// established peer's block reaches nobody; the product is fixed by the
    /// first report, so one naming another is not this pad's.
    #[test]
    fn reports_are_forwarded_in_order_and_the_product_is_fixed() {
        let (mut forwarding, told) = forwarding();
        forwarding.report(
            3,
            &Inbound::Feature {
                product: Product::DualSense,
                feature: Feature::Calibration,
                report: CALIBRATION,
            },
        );
        forwarding.report(
            3,
            &Inbound::Input {
                product: Product::DualSense,
                report: *DS5,
            },
        );
        forwarding.report(3, &Inbound::TouchBlock);
        forwarding.report(
            3,
            &Inbound::Input {
                product: Product::DualShock4,
                report: *DS4,
            },
        );
        let told = told.lock().unwrap();
        assert_eq!(told.len(), 2, "{told:?}");
        assert!(
            matches!(
                told[0],
                Forwarded::Feature {
                    pad: 3,
                    product: Product::DualSense,
                    len: 41,
                    ref report
                } if &report[..41] == CALIBRATION
            ),
            "{:?}",
            told[0]
        );
        assert!(matches!(
            told[1],
            Forwarded::Input {
                pad: 3,
                product: Product::DualSense,
                report
            } if report == *DS5
        ));
    }

    /// **The pad's end is told after its last report**, on an unplug and on
    /// the guest's own end, once each; a pad never forwarded is nobody's to
    /// tell about.
    #[test]
    fn a_pads_end_is_told_once_and_the_guests_end_tells_the_rest() {
        let (mut forwarding, told) = forwarding();
        for pad in [3, 4] {
            forwarding.report(
                pad,
                &Inbound::Input {
                    product: Product::DualShock4,
                    report: *DS4,
                },
            );
        }
        assert!(forwarding.unplug(3));
        assert!(!forwarding.unplug(3), "told twice");
        assert!(!forwarding.unplug(9), "a pad never forwarded");
        drop(forwarding);
        let told = told.lock().unwrap();
        let ends: Vec<u32> = told
            .iter()
            .filter_map(|f| match f {
                Forwarded::Unplugged { pad, .. } => Some(*pad),
                _ => None,
            })
            .collect();
        assert_eq!(ends, [3, 4]);
        assert_eq!(told.len(), 4);
    }
}
