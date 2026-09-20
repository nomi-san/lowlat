//! A controller's own reports (docs/01-protocol.md 11.1 and 11.2).
//!
//! A DualShock 4 or a DualSense says more than the sixteen-button layout
//! carries: touch contacts and motion on the way in, the lightbar and the
//! trigger effects on the way back. So its report travels raw -- opcode 31
//! with the input report a peer's pad produced, opcode 33 with what the host's
//! device was written -- and a host presents a device of the same model that
//! produces the same report. This module is the framing and the pure reads:
//! which product, which form, the standard state a report implies, and the
//! framing a wireless pad adds to its reports and expects back. Nothing here
//! owns a device.
//!
//! **The wire form is the USB form, identifier byte first**, for both
//! directions and both products, with one exception the wire document
//! explains: a DualShock 4's input body travels without its identifier byte,
//! because an established host in DualSense mode writes any 64-byte body on
//! opcode 31 into its virtual DualSense, and a DualShock 4's report is 64
//! bytes with the same identifier.

use crate::control::{self, Control};
use crate::crc32;
use crate::error::{Error, Result};

/// The products with a raw report path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Product {
    DualShock4,
    DualSense,
}

impl Product {
    /// The product identifier a peer names and a host presents.
    ///
    /// Both DualShock 4 generations are named as the second; a host presents
    /// that one.
    #[must_use]
    pub const fn product_id(self) -> u16 {
        match self {
            Product::DualShock4 => 0x09CC,
            Product::DualSense => 0x0CE6,
        }
    }

    /// The vendor both products share.
    pub const VENDOR_ID: u16 = 0x054C;

    /// The product a peer named, or `None` for one this path does not carry.
    #[must_use]
    pub fn from_product_id(id: u16) -> Option<Self> {
        match id {
            0x09CC | 0x05C4 => Some(Product::DualShock4),
            0x0CE6 => Some(Product::DualSense),
            _ => None,
        }
    }

    /// The device name the real product carries, for a host presenting one.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Product::DualShock4 => "Sony Interactive Entertainment Wireless Controller",
            Product::DualSense => "Sony Interactive Entertainment DualSense Wireless Controller",
        }
    }

    /// How large the product's output report is in the USB form, identifier
    /// included.
    #[must_use]
    pub const fn output_len(self) -> usize {
        match self {
            Product::DualShock4 => DS4_OUTPUT_LEN,
            Product::DualSense => DS5_OUTPUT_LEN,
        }
    }

    /// The identifier byte of the product's output report in the USB form.
    #[must_use]
    pub const fn output_id(self) -> u8 {
        match self {
            Product::DualShock4 => DS4_OUTPUT_ID,
            Product::DualSense => DS5_OUTPUT_ID,
        }
    }

    const fn bt_input_id(self) -> u8 {
        match self {
            Product::DualShock4 => DS4_BT_INPUT_ID,
            Product::DualSense => DS5_BT_INPUT_ID,
        }
    }

    const fn bt_output_id(self) -> u8 {
        match self {
            Product::DualShock4 => DS4_BT_OUTPUT_ID,
            Product::DualSense => DS5_BT_OUTPUT_ID,
        }
    }
}

/// How a peer's pad is attached, read off the reports it delivers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
}

/// The third argument of an inbound report: the product identifier in the low
/// sixteen bits, and this bit set when the body is a feature report rather
/// than an input report. Established peers write zero for the whole word.
pub const FEATURE_BIT: u32 = 1 << 16;

/// An input report in the USB form, identifier included; both products.
pub const INPUT_LEN: usize = 64;
/// The identifier byte of an input report in the USB form; both products.
pub const USB_INPUT_ID: u8 = 0x01;
/// A DualShock 4's input body on the wire: the USB report without its
/// identifier byte.
pub const DS4_BODY_LEN: usize = 63;
/// Where the block an established host's DualShock mode reads sits in the USB
/// report: the touch report count and the first touch report.
pub const DS4_TOUCH_AT: usize = 33;
/// How long that block is.
pub const DS4_TOUCH_LEN: usize = 10;

/// A wireless input report; both products.
pub const BT_INPUT_LEN: usize = 78;
const DS4_BT_INPUT_ID: u8 = 0x11;
const DS5_BT_INPUT_ID: u8 = 0x31;
/// Where the USB-form content sits inside a wireless input report.
const DS4_BT_INPUT_AT: usize = 3;
const DS5_BT_INPUT_AT: usize = 2;
/// How much of a DualShock 4's wireless report maps onto the USB form: the
/// common block, the count and the three touch reports; the fourth and the
/// padding do not exist in the USB form.
const DS4_BT_INPUT_MAPPED: usize = 61;

/// A DualShock 4's output report in the USB form.
pub const DS4_OUTPUT_ID: u8 = 0x05;
pub const DS4_OUTPUT_LEN: usize = 32;
/// A DualSense's output report in the USB form.
pub const DS5_OUTPUT_ID: u8 = 0x02;
pub const DS5_OUTPUT_LEN: usize = 48;
/// A wireless output report; both products.
pub const BT_OUTPUT_LEN: usize = 78;
const DS4_BT_OUTPUT_ID: u8 = 0x11;
const DS5_BT_OUTPUT_ID: u8 = 0x31;
/// Where the USB-form content sits inside a wireless output report.
const BT_OUTPUT_AT: usize = 3;
/// A DualShock 4's wireless output report asks for the checksum and marks
/// itself as one the device should apply.
const DS4_BT_OUTPUT_CONTROL: u8 = 0xC0;
/// A DualSense's wireless output report carries a tag after its sequence.
const DS5_BT_OUTPUT_TAG: u8 = 0x10;

/// The largest report either way, either framing.
pub const REPORT_MAX: usize = 78;
/// The largest feature report either product answers.
pub const FEATURE_MAX: usize = 64;

/// The wireless checksum's first byte, by the direction of the report.
const CRC_SEED_INPUT: u8 = 0xA1;
const CRC_SEED_OUTPUT: u8 = 0xA2;
const CRC_SEED_FEATURE: u8 = 0xA3;

/// The feature reports a peer sends ahead of its first input report: what a
/// host's driver asks a new device for, and scales the motion sensors by.
///
/// **Pairing is not here.** The host makes the device's address itself, so a
/// peer's pairing report would be replaced; it is refused rather than carried
/// and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Calibration,
    Firmware,
}

impl Feature {
    /// The report identifier, which is byte 0 of the report.
    #[must_use]
    pub const fn id(self, product: Product) -> u8 {
        match (self, product) {
            (Feature::Calibration, Product::DualShock4) => 0x02,
            (Feature::Firmware, Product::DualShock4) => 0xA3,
            (Feature::Calibration, Product::DualSense) => 0x05,
            (Feature::Firmware, Product::DualSense) => 0x20,
        }
    }

    /// The report's length, identifier included. Exact: a device answers
    /// these at one size each.
    #[must_use]
    pub const fn len(self, product: Product) -> usize {
        match (self, product) {
            (Feature::Calibration, Product::DualShock4) => 37,
            (Feature::Firmware, Product::DualShock4) => 49,
            (Feature::Calibration, Product::DualSense) => 41,
            (Feature::Firmware, Product::DualSense) => 64,
        }
    }

    /// Which feature report this is, by its identifier byte and its length, or
    /// `None` for anything else.
    #[must_use]
    pub fn of(product: Product, report: &[u8]) -> Option<Self> {
        let &id = report.first()?;
        [Feature::Calibration, Feature::Firmware]
            .into_iter()
            .find(|f| f.id(product) == id && f.len(product) == report.len())
    }
}

/// A DualShock 4 answers its calibration under another identifier and with a
/// checksum when it is wireless, and **with its gyro ranges in another
/// order**: the USB answer interleaves each axis's positive and negative range
/// (pitch, pitch, yaw, yaw, roll, roll), the wireless answer groups the three
/// positive ranges ahead of the three negative ones. A host's driver reads the
/// USB order from a USB device, so the answer is reordered as it is rewritten.
const DS4_BT_CALIBRATION_ID: u8 = 0x05;
const DS4_BT_CALIBRATION_LEN: usize = 41;
/// Where the six gyro ranges sit in either answer, two bytes each.
const DS4_CALIBRATION_RANGES_AT: usize = 7;

/// The whole-pad message's bit for each button (docs/01-protocol.md 11.1).
///
/// One representation is kept for the per-button message, the whole-pad
/// message and the raw report: the first converts into these bits on the way
/// in, the last is read into them, so a guest that uses more than one form has
/// one held state rather than two that disagree.
pub mod bit {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    /// A touchpad press. The sixteen-button pad has no such button and drops
    /// it; a pad made from its report has the press in the report.
    pub const TOUCHPAD: u16 = 0x0800;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

/// A whole pad at one moment, as the whole-pad message carries it: the
/// buttons as [`bit`]s, the sticks over the signed range with away from the
/// player positive, the triggers from zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    pub buttons: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

/// The standard state a report implies: what the sixteen-button message
/// would carry for the same moment.
///
/// **The same numbers a toolkit's own driver produces for the report**, so a
/// pad moved between the two paths reads the same: a stick byte `v` becomes
/// `v * 257 - 32768`, the vertical ones negated so that away from the player
/// is positive, the hat's eight directions become the four direction bits, and
/// the triggers pass through.
#[must_use]
pub fn state(product: Product, report: &[u8; INPUT_LEN]) -> State {
    let (buttons_at, triggers_at) = match product {
        Product::DualShock4 => (5, 8),
        Product::DualSense => (8, 5),
    };
    let byte = |at: usize| report.get(at).copied().unwrap_or(0);
    let b0 = byte(buttons_at);
    let b1 = byte(buttons_at + 1);
    let b2 = byte(buttons_at + 2);
    let mut buttons = hat(b0 & 0x0F);
    let bits = [
        (b0 & 0x10, bit::X),
        (b0 & 0x20, bit::A),
        (b0 & 0x40, bit::B),
        (b0 & 0x80, bit::Y),
        (b1 & 0x01, bit::LEFT_SHOULDER),
        (b1 & 0x02, bit::RIGHT_SHOULDER),
        (b1 & 0x10, bit::BACK),
        (b1 & 0x20, bit::START),
        (b1 & 0x40, bit::LEFT_THUMB),
        (b1 & 0x80, bit::RIGHT_THUMB),
        (b2 & 0x01, bit::GUIDE),
        (b2 & 0x02, bit::TOUCHPAD),
    ];
    for (set, b) in bits {
        if set != 0 {
            buttons |= b;
        }
    }
    State {
        buttons,
        lx: stick(byte(1), false),
        ly: stick(byte(2), true),
        rx: stick(byte(3), false),
        ry: stick(byte(4), true),
        lt: byte(triggers_at),
        rt: byte(triggers_at + 1),
    }
}

/// A stick byte over the signed range. `257 = 65535 / 255`, so the mapping is
/// exact and the extremes land on the range's ends.
const fn stick(v: u8, invert: bool) -> i16 {
    // v * 257 is in [0, 65535], so both expressions stay in the range and the
    // narrowing is a change of type, not of value.
    let d = v as u16;
    let d = d.wrapping_mul(257);
    if invert {
        (0x7FFF_u16.wrapping_sub(d)) as i16
    } else {
        (d ^ 0x8000) as i16
    }
}

/// The hat's eight directions as direction bits; anything else is centred.
const fn hat(v: u8) -> u16 {
    match v {
        0 => bit::DPAD_UP,
        1 => bit::DPAD_UP | bit::DPAD_RIGHT,
        2 => bit::DPAD_RIGHT,
        3 => bit::DPAD_RIGHT | bit::DPAD_DOWN,
        4 => bit::DPAD_DOWN,
        5 => bit::DPAD_DOWN | bit::DPAD_LEFT,
        6 => bit::DPAD_LEFT,
        7 => bit::DPAD_LEFT | bit::DPAD_UP,
        _ => 0,
    }
}

/// The block an established host's DualShock mode reads, out of the USB
/// report.
#[must_use]
pub fn ds4_touch_block(report: &[u8; INPUT_LEN]) -> [u8; DS4_TOUCH_LEN] {
    let mut block = [0u8; DS4_TOUCH_LEN];
    if let Some(src) = report.get(DS4_TOUCH_AT..DS4_TOUCH_AT + DS4_TOUCH_LEN) {
        block.copy_from_slice(src);
    }
    block
}

/// A DualShock 4's input body for the wire: the USB report without its
/// identifier byte.
#[must_use]
pub fn ds4_body(report: &[u8; INPUT_LEN]) -> &[u8] {
    report.get(1..).unwrap_or(&[])
}

/// The checksum a wireless report carries in its last four bytes: over the
/// direction's seed byte and everything before the checksum.
fn bt_crc(seed: u8, report: &[u8]) -> u32 {
    let mut crc = crc32::Crc32::new();
    crc.update(&[seed]);
    crc.update(report);
    crc.finish()
}

/// Whether a wireless report's checksum matches its content.
fn bt_checked(seed: u8, report: &[u8]) -> bool {
    let Some(at) = report.len().checked_sub(4) else {
        return false;
    };
    let (content, tail) = report.split_at(at);
    let Ok(tail) = <[u8; 4]>::try_from(tail) else {
        return false;
    };
    u32::from_le_bytes(tail) == bt_crc(seed, content)
}

/// Write the checksum into a wireless report's last four bytes.
fn bt_seal(seed: u8, report: &mut [u8]) {
    let Some(at) = report.len().checked_sub(4) else {
        return;
    };
    let crc = {
        let (content, _) = report.split_at(at);
        bt_crc(seed, content)
    };
    if let Some(tail) = report.get_mut(at..) {
        tail.copy_from_slice(&crc.to_le_bytes());
    }
}

/// Bring an input report, as the pad delivered it, to the USB form.
///
/// A USB report is copied. A wireless one is checked -- a report whose
/// checksum does not match is refused, as the driver refuses it -- and its
/// content moved to where the USB form has it. Anything else is refused.
pub fn normalize_input(
    product: Product,
    report: &[u8],
    out: &mut [u8; INPUT_LEN],
) -> Result<Transport> {
    let &id = report.first().ok_or(Error::ShortPacket)?;
    if id == USB_INPUT_ID && report.len() == INPUT_LEN {
        out.copy_from_slice(report);
        return Ok(Transport::Usb);
    }
    if id != product.bt_input_id() || report.len() != BT_INPUT_LEN {
        return Err(Error::Malformed);
    }
    if !bt_checked(CRC_SEED_INPUT, report) {
        return Err(Error::Decrypt);
    }
    out.fill(0);
    if let Some(first) = out.first_mut() {
        *first = USB_INPUT_ID;
    }
    let (from, len) = match product {
        Product::DualShock4 => (DS4_BT_INPUT_AT, DS4_BT_INPUT_MAPPED - 1),
        Product::DualSense => (DS5_BT_INPUT_AT, INPUT_LEN - 1),
    };
    if let (Some(dst), Some(src)) = (out.get_mut(1..1 + len), report.get(from..from + len)) {
        dst.copy_from_slice(src);
    }
    Ok(Transport::Bluetooth)
}

/// Bring a feature report, as the pad answered it, to the USB form.
///
/// Only the reports of [`Feature`] pass. A wireless pad answers with a
/// checksum in the last four bytes where the USB answer has zeros, and those
/// are zeroed when they verify; a wireless DualShock 4 answers its calibration
/// under another identifier as well, and that one is checked and rewritten.
/// Returns which report it is and its length in `out`.
pub fn normalize_feature(
    product: Product,
    report: &[u8],
    out: &mut [u8; FEATURE_MAX],
) -> Result<(Feature, usize)> {
    if product == Product::DualShock4
        && report.first() == Some(&DS4_BT_CALIBRATION_ID)
        && report.len() == DS4_BT_CALIBRATION_LEN
    {
        if !bt_checked(CRC_SEED_FEATURE, report) {
            return Err(Error::Decrypt);
        }
        let len = Feature::Calibration.len(product);
        let dst = out.get_mut(..len).ok_or(Error::BufferTooSmall)?;
        dst.copy_from_slice(report.get(..len).ok_or(Error::ShortPacket)?);
        if let Some(first) = dst.first_mut() {
            *first = Feature::Calibration.id(product);
        }
        // Grouped (+p +y +r -p -y -r) to interleaved (+p -p +y -y +r -r).
        let at = DS4_CALIBRATION_RANGES_AT;
        if let (Some(ranges), Some(grouped)) = (
            dst.get_mut(at..at + 12)
                .and_then(|r| <&mut [u8; 12]>::try_from(r).ok()),
            report
                .get(at..at + 12)
                .and_then(|r| <[u8; 12]>::try_from(r).ok()),
        ) {
            let [p0, p1, y0, y1, r0, r1, mp0, mp1, my0, my1, mr0, mr1] = grouped;
            *ranges = [p0, p1, mp0, mp1, y0, y1, my0, my1, r0, r1, mr0, mr1];
        }
        return Ok((Feature::Calibration, len));
    }
    let feature = Feature::of(product, report).ok_or(Error::Malformed)?;
    let dst = out.get_mut(..report.len()).ok_or(Error::BufferTooSmall)?;
    dst.copy_from_slice(report);
    if bt_checked(CRC_SEED_FEATURE, dst) {
        if let Some(tail) = dst.len().checked_sub(4).and_then(|at| dst.get_mut(at..)) {
            tail.fill(0);
        }
    }
    Ok((feature, report.len()))
}

/// Frame an output report in the USB form for the transport the pad is on.
///
/// A USB pad takes the report as it is. A wireless one takes it inside its own
/// report: the identifier and control bytes ahead, the content where that
/// report has it, the checksum last; a DualSense's also carries a sequence
/// number the caller advances per report. Returns the length in `out`.
pub fn frame_output(
    product: Product,
    transport: Transport,
    seq: u8,
    report: &[u8],
    out: &mut [u8; REPORT_MAX],
) -> Result<usize> {
    if report.first() != Some(&product.output_id()) || report.len() != product.output_len() {
        return Err(Error::Malformed);
    }
    match transport {
        Transport::Usb => {
            let dst = out.get_mut(..report.len()).ok_or(Error::BufferTooSmall)?;
            dst.copy_from_slice(report);
            Ok(report.len())
        }
        Transport::Bluetooth => {
            out.fill(0);
            let (b1, b2) = match product {
                Product::DualShock4 => (DS4_BT_OUTPUT_CONTROL, 0),
                Product::DualSense => (seq << 4, DS5_BT_OUTPUT_TAG),
            };
            if let Some(head) = out.get_mut(..BT_OUTPUT_AT) {
                head.copy_from_slice(&[product.bt_output_id(), b1, b2]);
            }
            let content = report.get(1..).ok_or(Error::ShortPacket)?;
            if let Some(dst) = out.get_mut(BT_OUTPUT_AT..BT_OUTPUT_AT + content.len()) {
                dst.copy_from_slice(content);
            }
            if let Some(framed) = out.get_mut(..BT_OUTPUT_LEN) {
                bt_seal(CRC_SEED_OUTPUT, framed);
            }
            Ok(BT_OUTPUT_LEN)
        }
    }
}

/// What an inbound report message carries, as a host reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound<'a> {
    /// A whole input report in the USB form, the DualShock 4's identifier
    /// byte restored.
    Input {
        product: Product,
        report: [u8; INPUT_LEN],
    },
    /// A feature report, identifier byte first.
    Feature {
        product: Product,
        feature: Feature,
        report: &'a [u8],
    },
    /// The ten-byte block an established peer's DualShock 4 sends. It names
    /// no product and creates nothing here.
    TouchBlock,
}

/// Read an inbound report message.
///
/// `None` for another opcode and for a body this path does not carry: a
/// product nobody named with a body that is neither an established peer's
/// DualSense report nor its touch block, a product with the wrong length, a
/// feature report that is not one of [`Feature`]. The declared length bounds
/// the body and never extends it.
#[must_use]
pub fn parse_report<'a>(message: &Control<'a>) -> Option<Inbound<'a>> {
    if message.opcode != control::op::PAD_REPORT {
        return None;
    }
    let declared = usize::try_from(message.a0).unwrap_or(usize::MAX);
    let body = message.body.get(..declared).unwrap_or(message.body);
    let product = u16::try_from(message.a2 & 0xFFFF)
        .ok()
        .and_then(Product::from_product_id);
    let feature = message.a2 & FEATURE_BIT != 0;
    match (product, feature) {
        (None, false) if body.len() == DS4_TOUCH_LEN => Some(Inbound::TouchBlock),
        (None, false) if body.len() == INPUT_LEN && body.first() == Some(&USB_INPUT_ID) => {
            Some(whole(Product::DualSense, body))
        }
        (None, _) => None,
        (Some(Product::DualShock4), false) if body.len() == DS4_BODY_LEN => {
            let mut report = [0u8; INPUT_LEN];
            if let Some(first) = report.first_mut() {
                *first = USB_INPUT_ID;
            }
            if let Some(rest) = report.get_mut(1..) {
                rest.copy_from_slice(body);
            }
            Some(Inbound::Input {
                product: Product::DualShock4,
                report,
            })
        }
        (Some(Product::DualSense), false)
            if body.len() == INPUT_LEN && body.first() == Some(&USB_INPUT_ID) =>
        {
            Some(whole(Product::DualSense, body))
        }
        (Some(product), true) => Feature::of(product, body).map(|feature| Inbound::Feature {
            product,
            feature,
            report: body,
        }),
        (Some(_), false) => None,
    }
}

fn whole<'a>(product: Product, body: &[u8]) -> Inbound<'a> {
    let mut report = [0u8; INPUT_LEN];
    if body.len() == INPUT_LEN {
        report.copy_from_slice(body);
    }
    Inbound::Input { product, report }
}

/// Write an inbound report message: the header with the product and the
/// feature bit, then the body.
pub fn encode_report(
    out: &mut [u8],
    pad: u32,
    product: Product,
    feature: bool,
    body: &[u8],
) -> Result<usize> {
    let flags = if feature { FEATURE_BIT } else { 0 };
    encode_with(
        out,
        control::op::PAD_REPORT,
        pad,
        u32::from(product.product_id()) | flags,
        body,
    )
}

/// Write the established peer's ten-byte block, which names no product.
pub fn encode_touch_block(out: &mut [u8], pad: u32, report: &[u8; INPUT_LEN]) -> Result<usize> {
    encode_with(
        out,
        control::op::PAD_REPORT,
        pad,
        0,
        &ds4_touch_block(report),
    )
}

/// What an outbound message carries: what the host's device was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// A feature report written to the device.
    Feature,
    /// An output report: motors, lights, effects.
    Output,
}

impl OutputKind {
    const fn wire(self) -> u32 {
        match self {
            OutputKind::Feature => 0,
            OutputKind::Output => 1,
        }
    }

    fn from_wire(kind: u32) -> Option<Self> {
        match kind {
            0 => Some(OutputKind::Feature),
            1 => Some(OutputKind::Output),
            _ => None,
        }
    }
}

/// An outbound report message, as a peer reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Output<'a> {
    pub pad: u32,
    pub kind: OutputKind,
    /// The report in the USB form, identifier byte first.
    pub report: &'a [u8],
}

/// Read an outbound report message. `None` for another opcode, a kind nobody
/// defined, or an empty body.
#[must_use]
pub fn parse_output<'a>(message: &Control<'a>) -> Option<Output<'a>> {
    if message.opcode != control::op::PAD_OUTPUT {
        return None;
    }
    let declared = usize::try_from(message.a0).unwrap_or(usize::MAX);
    let report = message.body.get(..declared).unwrap_or(message.body);
    if report.is_empty() {
        return None;
    }
    Some(Output {
        pad: message.a2,
        kind: OutputKind::from_wire(message.a1)?,
        report,
    })
}

/// Write an outbound report message: the length, the kind, the pad, then the
/// report.
pub fn encode_output(out: &mut [u8], pad: u32, kind: OutputKind, report: &[u8]) -> Result<usize> {
    encode_with(out, control::op::PAD_OUTPUT, kind.wire(), pad, report)
}

/// A header whose first argument is the body's length, then the body.
fn encode_with(out: &mut [u8], opcode: u8, a1: u32, a2: u32, body: &[u8]) -> Result<usize> {
    let a0 = u32::try_from(body.len()).map_err(|_| Error::Oversized)?;
    let header = control::encode_header(
        out,
        &Control {
            a0,
            a1,
            a2,
            opcode,
            body: &[],
        },
    )?;
    let dst = out
        .get_mut(header..header + body.len())
        .ok_or(Error::BufferTooSmall)?;
    dst.copy_from_slice(body);
    Ok(header + body.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DS4_IDLE: &[u8; INPUT_LEN] = include_bytes!("../tests/data/pad/ds4/input-idle.bin");
    const DS5_IDLE: &[u8; INPUT_LEN] = include_bytes!("../tests/data/pad/ds5/input-idle.bin");
    const DS4_CALIBRATION: &[u8] = include_bytes!("../tests/data/pad/ds4/feature-calibration.bin");
    const DS4_FIRMWARE: &[u8] = include_bytes!("../tests/data/pad/ds4/feature-firmware.bin");
    const DS5_CALIBRATION: &[u8] = include_bytes!("../tests/data/pad/ds5/feature-calibration.bin");
    const DS5_FIRMWARE: &[u8] = include_bytes!("../tests/data/pad/ds5/feature-firmware.bin");
    const DS5_PAIRING: &[u8] = include_bytes!("../tests/data/pad/ds5/feature-pairing.bin");
    const DS4_HELD: &[u8; INPUT_LEN] = include_bytes!("../tests/data/pad/ds4/input-held.bin");
    const DS5_HELD: &[u8; INPUT_LEN] = include_bytes!("../tests/data/pad/ds5/input-held.bin");
    const DS5_BT_IDLE: &[u8; BT_INPUT_LEN] =
        include_bytes!("../tests/data/pad/ds5/bt-input-idle.bin");
    const DS5_BT_CALIBRATION: &[u8] =
        include_bytes!("../tests/data/pad/ds5/bt-feature-calibration.bin");
    const DS5_BT_FIRMWARE: &[u8] = include_bytes!("../tests/data/pad/ds5/bt-feature-firmware.bin");
    const DS4_BT_IDLE: &[u8; BT_INPUT_LEN] =
        include_bytes!("../tests/data/pad/ds4/bt-input-idle.bin");
    const DS4_BT_CALIBRATION: &[u8] =
        include_bytes!("../tests/data/pad/ds4/bt-feature-calibration.bin");
    const DS4_BT_FIRMWARE: &[u8] = include_bytes!("../tests/data/pad/ds4/bt-feature-firmware.bin");

    /// A report a real pad produced, at rest: every stick centred, nothing
    /// pressed, the hat neutral. The two products centre their sticks one
    /// count apart, which is what the pads do and not a mistake in the read.
    #[test]
    fn a_pad_at_rest_reads_as_nothing_held() {
        let ds4 = state(Product::DualShock4, DS4_IDLE);
        assert_eq!(ds4.buttons, 0);
        assert_eq!((ds4.lt, ds4.rt), (0, 0));
        assert_eq!((ds4.lx, ds4.ly, ds4.rx, ds4.ry), (128, -129, 128, -129));

        let ds5 = state(Product::DualSense, DS5_IDLE);
        assert_eq!(ds5.buttons, 0);
        assert_eq!((ds5.lt, ds5.rt), (0, 0));
        assert_eq!((ds5.lx, ds5.ly, ds5.rx, ds5.ry), (-129, 128, 128, -129));
    }

    /// A report each pad produced with a finger on the touchpad and Cross
    /// held: Cross is the one button, and the first touch contact is live at
    /// the offset each product keeps it.
    #[test]
    fn a_held_pad_reads_cross_and_a_live_contact() {
        assert_eq!(state(Product::DualShock4, DS4_HELD).buttons, bit::A);
        assert_eq!(DS4_HELD[DS4_TOUCH_AT], 1);
        assert_eq!(DS4_HELD[DS4_TOUCH_AT + 2] & 0x80, 0);
        assert_ne!(DS4_HELD[DS4_TOUCH_AT + 6] & 0x80, 0);
        assert_eq!(state(Product::DualSense, DS5_HELD).buttons, bit::A);
        assert_eq!(DS5_HELD[33] & 0x80, 0);
        assert_ne!(DS5_HELD[37] & 0x80, 0);
    }

    /// What the DualSense on the desk sent over Bluetooth: the checksum
    /// verifies under the input seed, the content normalises to a report at
    /// rest, and the calibration and firmware answers it gave over Bluetooth
    /// are, once their checksums are stripped, byte for byte the answers it
    /// gave over USB -- which ties the seeds and the offsets to the device
    /// rather than to each other.
    #[test]
    fn the_wireless_dualsense_agrees_with_itself_over_usb() {
        let mut out = [0u8; INPUT_LEN];
        assert_eq!(
            normalize_input(Product::DualSense, DS5_BT_IDLE, &mut out).unwrap(),
            Transport::Bluetooth
        );
        let s = state(Product::DualSense, &out);
        assert_eq!(s.buttons, 0);
        assert_eq!((s.lt, s.rt), (0, 0));
        for v in [s.lx, s.ly, s.rx, s.ry] {
            assert!(v.abs() < 1000, "stick at rest reads {v}");
        }

        let mut feature = [0u8; FEATURE_MAX];
        assert_eq!(
            normalize_feature(Product::DualSense, DS5_BT_CALIBRATION, &mut feature).unwrap(),
            (Feature::Calibration, 41)
        );
        assert_eq!(&feature[..41], DS5_CALIBRATION);
        assert_ne!(&DS5_BT_CALIBRATION[37..], &[0, 0, 0, 0]);
        assert_eq!(
            normalize_feature(Product::DualSense, DS5_BT_FIRMWARE, &mut feature).unwrap(),
            (Feature::Firmware, 64)
        );
        assert_eq!(&feature[..64], DS5_FIRMWARE);
    }

    /// What the DualShock 4 on the desk sent over Bluetooth: the input
    /// verifies under the input seed and normalises to a pad at rest with the
    /// touch count where the USB form keeps it; the calibration answer
    /// verifies under the feature seed and is rewritten to the USB identifier
    /// with its gyro ranges interleaved -- this pad answers nominal ranges over
    /// Bluetooth, three positive then three negative, which is exactly the
    /// grouping the rewrite must undo; the firmware answer carries no
    /// checksum and passes through, and it is not the USB one: the pad
    /// reports another build over the air.
    #[test]
    fn the_wireless_dualshock_normalises_and_its_calibration_is_reordered() {
        let mut out = [0u8; INPUT_LEN];
        assert_eq!(
            normalize_input(Product::DualShock4, DS4_BT_IDLE, &mut out).unwrap(),
            Transport::Bluetooth
        );
        let s = state(Product::DualShock4, &out);
        assert_eq!(s.buttons, 0);
        assert_eq!((s.lt, s.rt), (0, 0));
        assert_eq!((s.lx, s.ly, s.rx, s.ry), (128, -129, 128, -129));
        assert_eq!(out[DS4_TOUCH_AT], 1);
        assert_ne!(out[DS4_TOUCH_AT + 2] & 0x80, 0);

        let mut feature = [0u8; FEATURE_MAX];
        assert_eq!(
            normalize_feature(Product::DualShock4, DS4_BT_CALIBRATION, &mut feature).unwrap(),
            (Feature::Calibration, 37)
        );
        assert_eq!(feature[0], 0x02);
        assert_eq!(&feature[1..7], &DS4_BT_CALIBRATION[1..7]);
        // Grouped on the air: +0x2200 x3, then -0x2200 x3.
        assert_eq!(
            &DS4_BT_CALIBRATION[7..19],
            &[0, 0x22, 0, 0x22, 0, 0x22, 0, 0xde, 0, 0xde, 0, 0xde]
        );
        // Interleaved for the driver: +, -, +, -, +, -.
        assert_eq!(
            &feature[7..19],
            &[0, 0x22, 0, 0xde, 0, 0x22, 0, 0xde, 0, 0x22, 0, 0xde]
        );
        assert_eq!(&feature[19..37], &DS4_BT_CALIBRATION[19..37]);
        assert_eq!(
            normalize_feature(Product::DualShock4, DS4_BT_FIRMWARE, &mut feature).unwrap(),
            (Feature::Firmware, 49)
        );
        assert_eq!(&feature[..49], DS4_BT_FIRMWARE);
        assert_ne!(DS4_BT_FIRMWARE, DS4_FIRMWARE);
    }

    /// The button bits of each product's report, read into the one bit set
    /// both messages share. Each bit is placed by hand at the offsets the
    /// device documents and read back as exactly one button.
    #[test]
    fn every_button_bit_lands_on_its_own_button() {
        for (product, at) in [(Product::DualShock4, 5usize), (Product::DualSense, 8usize)] {
            let cases: [(usize, u8, u16); 12] = [
                (0, 0x10, bit::X),
                (0, 0x20, bit::A),
                (0, 0x40, bit::B),
                (0, 0x80, bit::Y),
                (1, 0x01, bit::LEFT_SHOULDER),
                (1, 0x02, bit::RIGHT_SHOULDER),
                (1, 0x10, bit::BACK),
                (1, 0x20, bit::START),
                (1, 0x40, bit::LEFT_THUMB),
                (1, 0x80, bit::RIGHT_THUMB),
                (2, 0x01, bit::GUIDE),
                (2, 0x02, bit::TOUCHPAD),
            ];
            for (offset, mask, expected) in cases {
                let mut report = [0u8; INPUT_LEN];
                report[at] = 0x08; // hat centred
                report[at + offset] |= mask;
                assert_eq!(
                    state(product, &report).buttons,
                    expected,
                    "{product:?} byte {offset} mask {mask:#x}"
                );
            }
        }
    }

    /// The hat's eight directions, and the ninth that means centred.
    #[test]
    fn the_hat_becomes_direction_bits() {
        let expected = [
            bit::DPAD_UP,
            bit::DPAD_UP | bit::DPAD_RIGHT,
            bit::DPAD_RIGHT,
            bit::DPAD_RIGHT | bit::DPAD_DOWN,
            bit::DPAD_DOWN,
            bit::DPAD_DOWN | bit::DPAD_LEFT,
            bit::DPAD_LEFT,
            bit::DPAD_LEFT | bit::DPAD_UP,
            0,
        ];
        for (v, want) in expected.into_iter().enumerate() {
            let mut report = [0u8; INPUT_LEN];
            report[5] = v as u8;
            assert_eq!(state(Product::DualShock4, &report).buttons, want, "hat {v}");
        }
    }

    /// The stick mapping is the toolkit's: exact at the ends, the vertical
    /// axes negated so away from the player is positive.
    #[test]
    fn sticks_span_the_signed_range_with_up_positive() {
        let mut report = [0u8; INPUT_LEN];
        report[1..5].copy_from_slice(&[0, 0, 255, 255]);
        let s = state(Product::DualSense, &report);
        assert_eq!((s.lx, s.ly), (-32768, 32767));
        assert_eq!((s.rx, s.ry), (32767, -32768));
        report[5] = 200;
        report[6] = 7;
        assert_eq!(
            (
                state(Product::DualSense, &report).lt,
                state(Product::DualSense, &report).rt
            ),
            (200, 7)
        );
        let mut ds4 = [0u8; INPUT_LEN];
        ds4[8] = 200;
        ds4[9] = 7;
        assert_eq!(
            (
                state(Product::DualShock4, &ds4).lt,
                state(Product::DualShock4, &ds4).rt
            ),
            (200, 7)
        );
    }

    /// The block an established host reads is the count and the first touch
    /// report, at the offset that host reads it from.
    #[test]
    fn the_touch_block_is_the_ten_bytes_at_the_established_offset() {
        let block = ds4_touch_block(DS4_IDLE);
        assert_eq!(&block, &DS4_IDLE[33..43]);
        // At rest: one touch report, both contacts marked absent.
        assert_eq!(block[0], 1);
        assert_ne!(block[2] & 0x80, 0);
        assert_ne!(block[6] & 0x80, 0);
    }

    /// The wireless framing round-trips through the USB form: what a wireless
    /// pad would deliver for the same moment reads the same, and the framing
    /// a wireless pad expects back carries the USB content where the device
    /// reads it, under a checksum that verifies.
    #[test]
    fn wireless_input_normalises_to_the_usb_form_and_a_bad_checksum_is_refused() {
        // DualSense: the USB content sits at 2 in the wireless report.
        let mut bt = [0u8; BT_INPUT_LEN];
        bt[0] = 0x31;
        bt[1] = 7;
        bt[2..65].copy_from_slice(&DS5_IDLE[1..]);
        bt_seal(CRC_SEED_INPUT, &mut bt);
        let mut out = [0u8; INPUT_LEN];
        assert_eq!(
            normalize_input(Product::DualSense, &bt, &mut out).unwrap(),
            Transport::Bluetooth
        );
        assert_eq!(&out, DS5_IDLE);
        bt[10] ^= 1;
        assert_eq!(
            normalize_input(Product::DualSense, &bt, &mut out),
            Err(Error::Decrypt)
        );

        // DualShock 4: the common block at 3, the count and touch reports after.
        let mut bt = [0u8; BT_INPUT_LEN];
        bt[0] = 0x11;
        bt[3..63].copy_from_slice(&DS4_IDLE[1..61]);
        bt_seal(CRC_SEED_INPUT, &mut bt);
        assert_eq!(
            normalize_input(Product::DualShock4, &bt, &mut out).unwrap(),
            Transport::Bluetooth
        );
        assert_eq!(&out[..61], &DS4_IDLE[..61]);
        assert_eq!(&out[61..], &[0, 0, 0]);
        assert_eq!(
            state(Product::DualShock4, &out),
            state(Product::DualShock4, DS4_IDLE)
        );

        // A USB report passes through and says so.
        assert_eq!(
            normalize_input(Product::DualShock4, DS4_IDLE, &mut out).unwrap(),
            Transport::Usb
        );
        assert_eq!(&out, DS4_IDLE);
        // The other product's wireless identifier, and a wrong length, are refused.
        assert_eq!(
            normalize_input(Product::DualShock4, &[0x31; BT_INPUT_LEN], &mut out),
            Err(Error::Malformed)
        );
        assert_eq!(
            normalize_input(Product::DualSense, &DS5_IDLE[..63], &mut out),
            Err(Error::Malformed)
        );
    }

    #[test]
    fn output_reports_are_framed_for_the_transport() {
        let mut usb = [0u8; DS5_OUTPUT_LEN];
        usb[0] = DS5_OUTPUT_ID;
        usb[1] = 0x03;
        usb[3] = 200;
        usb[4] = 100;
        usb[45..48].copy_from_slice(&[255, 0, 255]);
        let mut out = [0u8; REPORT_MAX];
        assert_eq!(
            frame_output(Product::DualSense, Transport::Usb, 0, &usb, &mut out).unwrap(),
            DS5_OUTPUT_LEN
        );
        assert_eq!(&out[..DS5_OUTPUT_LEN], &usb);

        let n = frame_output(Product::DualSense, Transport::Bluetooth, 5, &usb, &mut out).unwrap();
        assert_eq!(n, BT_OUTPUT_LEN);
        assert_eq!(&out[..3], &[0x31, 0x50, 0x10]);
        assert_eq!(&out[3..50], &usb[1..]);
        assert!(bt_checked(CRC_SEED_OUTPUT, &out[..n]));

        let mut ds4 = [0u8; DS4_OUTPUT_LEN];
        ds4[0] = DS4_OUTPUT_ID;
        ds4[1] = 0x07;
        ds4[4..9].copy_from_slice(&[10, 20, 0, 0, 64]);
        let n = frame_output(Product::DualShock4, Transport::Bluetooth, 9, &ds4, &mut out).unwrap();
        assert_eq!(n, BT_OUTPUT_LEN);
        assert_eq!(&out[..3], &[0x11, 0xC0, 0]);
        assert_eq!(&out[3..34], &ds4[1..]);
        assert!(bt_checked(CRC_SEED_OUTPUT, &out[..n]));

        // The wrong product's report, or the wrong length, is refused.
        assert_eq!(
            frame_output(Product::DualShock4, Transport::Usb, 0, &usb, &mut out),
            Err(Error::Malformed)
        );
        assert_eq!(
            frame_output(Product::DualSense, Transport::Usb, 0, &usb[..47], &mut out),
            Err(Error::Malformed)
        );
    }

    /// The feature reports the pads on the desk answered, recognised by
    /// identifier and length; the pairing report is not carried.
    #[test]
    fn the_feature_reports_are_the_two_the_driver_scales_by() {
        assert_eq!(
            Feature::of(Product::DualShock4, DS4_CALIBRATION),
            Some(Feature::Calibration)
        );
        assert_eq!(
            Feature::of(Product::DualShock4, DS4_FIRMWARE),
            Some(Feature::Firmware)
        );
        assert_eq!(
            Feature::of(Product::DualSense, DS5_CALIBRATION),
            Some(Feature::Calibration)
        );
        assert_eq!(
            Feature::of(Product::DualSense, DS5_FIRMWARE),
            Some(Feature::Firmware)
        );
        assert_eq!(Feature::of(Product::DualSense, DS5_PAIRING), None);
        assert_eq!(Feature::of(Product::DualSense, DS4_CALIBRATION), None);
        assert_eq!(Feature::of(Product::DualShock4, &DS4_FIRMWARE[..48]), None);
        assert_eq!(Feature::of(Product::DualShock4, &[]), None);
        for (f, p) in [
            (Feature::Calibration, Product::DualShock4),
            (Feature::Firmware, Product::DualShock4),
            (Feature::Calibration, Product::DualSense),
            (Feature::Firmware, Product::DualSense),
        ] {
            assert!(f.len(p) <= FEATURE_MAX);
        }
    }

    /// A wireless DualShock 4's calibration answer is rewritten to the USB
    /// one: the USB identifier, no checksum, the gyro ranges interleaved from
    /// their grouped order, everything else as it was.
    #[test]
    fn a_wireless_ds4_calibration_is_rewritten_to_the_usb_report() {
        let mut bt = [0u8; DS4_BT_CALIBRATION_LEN];
        bt[0] = DS4_BT_CALIBRATION_ID;
        bt[1..37].copy_from_slice(&DS4_CALIBRATION[1..]);
        // Group the USB fixture's interleaved ranges as the air carries them.
        for axis in 0..3 {
            bt[7 + axis * 2..9 + axis * 2]
                .copy_from_slice(&DS4_CALIBRATION[7 + axis * 4..9 + axis * 4]);
            bt[13 + axis * 2..15 + axis * 2]
                .copy_from_slice(&DS4_CALIBRATION[9 + axis * 4..11 + axis * 4]);
        }
        bt_seal(CRC_SEED_FEATURE, &mut bt);
        let mut out = [0u8; FEATURE_MAX];
        assert_eq!(
            normalize_feature(Product::DualShock4, &bt, &mut out).unwrap(),
            (Feature::Calibration, 37)
        );
        assert_eq!(&out[..37], DS4_CALIBRATION);
        bt[20] ^= 1;
        assert_eq!(
            normalize_feature(Product::DualShock4, &bt, &mut out),
            Err(Error::Decrypt)
        );
        assert_eq!(
            normalize_feature(Product::DualSense, DS5_FIRMWARE, &mut out).unwrap(),
            (Feature::Firmware, 64)
        );
        assert_eq!(&out, DS5_FIRMWARE);
        assert_eq!(
            normalize_feature(Product::DualSense, DS5_PAIRING, &mut out),
            Err(Error::Malformed)
        );
    }

    fn parse(buf: &[u8]) -> Control<'_> {
        control::parse(buf).unwrap()
    }

    /// Both products' input reports round-trip through the wire form: the
    /// DualShock 4's loses its identifier on the wire and gets it back.
    #[test]
    fn input_reports_round_trip_and_the_ds4_travels_without_its_identifier() {
        let mut buf = [0u8; 128];
        let n = encode_report(&mut buf, 7, Product::DualShock4, false, ds4_body(DS4_IDLE)).unwrap();
        assert_eq!(n, control::CONTROL_HEADER_LEN + DS4_BODY_LEN);
        let message = parse(&buf[..n]);
        assert_eq!((message.a0, message.a1, message.a2), (63, 7, 0x09CC));
        assert_eq!(
            parse_report(&message),
            Some(Inbound::Input {
                product: Product::DualShock4,
                report: *DS4_IDLE
            })
        );

        let n = encode_report(&mut buf, 3, Product::DualSense, false, DS5_IDLE).unwrap();
        let message = parse(&buf[..n]);
        assert_eq!((message.a0, message.a1, message.a2), (64, 3, 0x0CE6));
        assert_eq!(
            parse_report(&message),
            Some(Inbound::Input {
                product: Product::DualSense,
                report: *DS5_IDLE
            })
        );
    }

    /// An established peer names no product: its 64-byte report is a
    /// DualSense's, its ten-byte block is a DualShock 4's touch block, and a
    /// DualShock 4's whole report at 64 bytes would be read as a DualSense's
    /// -- which is why ours travels at 63.
    #[test]
    fn an_established_peers_reports_are_told_apart_by_length() {
        let mut buf = [0u8; 128];
        let n = encode_with(&mut buf, control::op::PAD_REPORT, 1, 0, DS5_IDLE).unwrap();
        assert_eq!(
            parse_report(&parse(&buf[..n])),
            Some(Inbound::Input {
                product: Product::DualSense,
                report: *DS5_IDLE
            })
        );
        let n = encode_touch_block(&mut buf, 1, DS4_IDLE).unwrap();
        let message = parse(&buf[..n]);
        assert_eq!((message.a0, message.a2), (10, 0));
        assert_eq!(parse_report(&message), Some(Inbound::TouchBlock));
        // Anything else without a product is nothing.
        let n = encode_with(&mut buf, control::op::PAD_REPORT, 1, 0, &DS5_IDLE[..40]).unwrap();
        assert_eq!(parse_report(&parse(&buf[..n])), None);
    }

    /// A product with the wrong length, a feature report nobody defined, and
    /// another opcode all read as nothing.
    #[test]
    fn the_wrong_shape_for_a_named_product_is_refused() {
        let mut buf = [0u8; 128];
        let n = encode_report(&mut buf, 1, Product::DualShock4, false, DS4_IDLE).unwrap();
        assert_eq!(parse_report(&parse(&buf[..n])), None);
        let n = encode_report(&mut buf, 1, Product::DualSense, false, ds4_body(DS5_IDLE)).unwrap();
        assert_eq!(parse_report(&parse(&buf[..n])), None);
        let n = encode_report(&mut buf, 1, Product::DualSense, true, DS5_PAIRING).unwrap();
        assert_eq!(parse_report(&parse(&buf[..n])), None);
        let n = encode_report(&mut buf, 1, Product::DualSense, true, DS5_CALIBRATION).unwrap();
        let message = parse(&buf[..n]);
        assert_eq!(message.a2, 0x0CE6 | FEATURE_BIT);
        assert_eq!(
            parse_report(&message),
            Some(Inbound::Feature {
                product: Product::DualSense,
                feature: Feature::Calibration,
                report: DS5_CALIBRATION
            })
        );
        let mut other = message;
        other.opcode = control::op::GAMEPAD_STATE;
        assert_eq!(parse_report(&other), None);
    }

    /// The declared length bounds the body and never extends it.
    #[test]
    fn a_declared_length_bounds_the_body() {
        let mut buf = [0u8; 128];
        let n = encode_report(&mut buf, 1, Product::DualSense, false, DS5_IDLE).unwrap();
        let mut message = parse(&buf[..n]);
        message.a0 = 4096;
        assert!(matches!(
            parse_report(&message),
            Some(Inbound::Input { .. })
        ));
        // Shortened, the body is taken at its word: ten bytes of a DualSense
        // report are not a DualSense report, and without a product they are
        // the established peer's block.
        message.a0 = 10;
        assert_eq!(parse_report(&message), None);
        message.a2 = 0;
        assert_eq!(parse_report(&message), Some(Inbound::TouchBlock));
    }

    #[test]
    fn output_messages_round_trip_with_the_kind_and_the_pad() {
        let mut buf = [0u8; 128];
        let report = [DS5_OUTPUT_ID, 3, 0, 200, 100];
        let n = encode_output(&mut buf, 9, OutputKind::Output, &report).unwrap();
        let message = parse(&buf[..n]);
        assert_eq!((message.a0, message.a1, message.a2), (5, 1, 9));
        assert_eq!(
            parse_output(&message),
            Some(Output {
                pad: 9,
                kind: OutputKind::Output,
                report: &report
            })
        );
        let n = encode_output(&mut buf, 9, OutputKind::Feature, &report).unwrap();
        assert_eq!(
            parse_output(&parse(&buf[..n])).unwrap().kind,
            OutputKind::Feature
        );
        let mut message = parse(&buf[..n]);
        message.a1 = 7;
        assert_eq!(parse_output(&message), None);
        message.a1 = 1;
        message.a0 = 0;
        assert_eq!(parse_output(&message), None);
        message.opcode = control::op::RUMBLE;
        assert_eq!(parse_output(&message), None);
    }

    #[test]
    fn a_buffer_too_small_for_the_body_is_refused() {
        let mut buf = [0u8; 40];
        assert_eq!(
            encode_report(&mut buf, 1, Product::DualSense, false, DS5_IDLE),
            Err(Error::BufferTooSmall)
        );
        assert_eq!(
            encode_output(&mut buf, 1, OutputKind::Output, &[0u8; 60]),
            Err(Error::BufferTooSmall)
        );
    }

    #[test]
    fn products_are_named_by_identifier() {
        assert_eq!(Product::from_product_id(0x09CC), Some(Product::DualShock4));
        assert_eq!(Product::from_product_id(0x05C4), Some(Product::DualShock4));
        assert_eq!(Product::from_product_id(0x0CE6), Some(Product::DualSense));
        assert_eq!(Product::from_product_id(0x0268), None);
        assert_eq!(Product::DualShock4.product_id(), 0x09CC);
        assert_eq!(Product::DualSense.product_id(), 0x0CE6);
    }
}
