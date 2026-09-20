//! Phase 1 gate 2: encode then decode is the identity, across the field ranges.
//!
//! A deterministic generator rather than a property-testing dependency. Ten
//! thousand cases from a fixed seed reproduce exactly, which matters more here
//! than shrinking: a failure is replayed by running the test again, and the
//! corpus already covers the shapes that occur in practice. What this adds is
//! the field values that do *not* occur in practice, including the boundaries.

// The generator narrows deliberately: a case index becomes a channel or a
// dimension, and a truncating cast there is the point rather than a hazard.
#![allow(clippy::cast_possible_truncation)]

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::envelope::Envelope;
use lowlat_core::message::{self, Message};
use lowlat_core::packet::{self, Ack, AckKind, CHANNEL_COUNT, Data, Packet};
use lowlat_core::pad::{self, Feature, Inbound, Output, OutputKind, Product, Transport};
use lowlat_core::{control, crc32, video};

const CASES: usize = 10_000;

/// Deterministic and seeded, so a failure replays.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64star
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u32) -> u32 {
        if bound == 0 {
            0
        } else {
            (self.next() % u64::from(bound)) as u32
        }
    }

    fn byte(&mut self) -> u8 {
        (self.next() & 0xFF) as u8
    }

    /// Biased toward boundaries, which is where encoders break.
    fn interesting_u32(&mut self) -> u32 {
        match self.next() % 8 {
            0 => 0,
            1 => 1,
            2 => u32::MAX - 1,
            3 => i32::MAX as u32,
            4 => (i32::MAX as u32).wrapping_add(1),
            5 => 0xFFFF,
            _ => (self.next() >> 32) as u32,
        }
    }

    fn fill(&mut self, buf: &mut [u8]) {
        for byte in buf.iter_mut() {
            *byte = self.byte();
        }
    }
}

#[test]
fn data_packets_round_trip() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF0);
    let mut body = [0u8; 1400];
    let mut buf = [0u8; 1500];

    for case in 0..CASES {
        let len = rng.below(1394) as usize;
        let body = &mut body[..len];
        rng.fill(body);
        // A sequence of all ones is reserved and refused by design.
        let seq = match rng.interesting_u32() {
            u32::MAX => 0,
            other => other,
        };
        let data = Data {
            channel: rng.below(CHANNEL_COUNT as u32) as u8,
            seq,
            last: rng.next() & 1 == 0,
            body,
        };
        let written = packet::encode_data(&mut buf, &data).expect("encode");
        let Packet::Data(decoded) = packet::parse(&buf[..written]).expect("parse") else {
            panic!("case {case}: not a data packet");
        };
        assert_eq!(decoded, data, "case {case}");
    }
}

#[test]
fn group_acknowledgements_round_trip() {
    let mut rng = Rng(0xDEAD_BEEF_CAFE_0001);
    let mut buf = [0u8; 128];

    for case in 0..CASES {
        let mut cumulative = [0u32; CHANNEL_COUNT];
        for slot in &mut cumulative {
            *slot = rng.interesting_u32();
        }
        let keepalive = rng.next() & 1 == 0;
        let ack = Ack {
            // Encoding always writes every channel, so a round trip reports
            // every channel back.
            reported: CHANNEL_COUNT,
            kind: if keepalive {
                AckKind::Keepalive
            } else {
                AckKind::Ack
            },
            // The negative acknowledgement bit is only legal with an ack.
            nack: !keepalive && rng.next() & 1 == 0,
            trigger_channel: rng.below(CHANNEL_COUNT as u32) as u8,
            trigger_seq: match rng.interesting_u32() {
                u32::MAX => 0,
                other => other,
            },
            cumulative,
        };
        let written = packet::encode_ack(&mut buf, &ack).expect("encode");
        let Packet::Ack(decoded) = packet::parse(&buf[..written]).expect("parse") else {
            panic!("case {case}: not an acknowledgement");
        };
        assert_eq!(decoded, ack, "case {case}");
    }
}

#[test]
fn control_headers_round_trip() {
    let mut rng = Rng(0x0BAD_F00D_0000_0007);
    let mut buf = [0u8; 64];

    for case in 0..CASES {
        let control = control::Control {
            a0: rng.interesting_u32(),
            a1: rng.interesting_u32(),
            a2: rng.interesting_u32(),
            opcode: rng.byte(),
            body: &[],
        };
        control::encode_header(&mut buf, &control).expect("encode");
        let decoded = control::parse(&buf[..control::CONTROL_HEADER_LEN]).expect("parse");
        assert_eq!(decoded, control, "case {case}");
    }
}

#[test]
fn video_headers_round_trip() {
    let mut rng = Rng(0xFEED_FACE_0000_0011);
    let mut buf = [0u8; 32];

    for case in 0..CASES {
        let header = video::VideoHeader {
            frame_id: rng.interesting_u32(),
            width: rng.below(0xFFFF) as u16,
            height: rng.below(0xFFFF) as u16,
            codec: if rng.next() & 1 == 0 {
                video::Codec::H264
            } else {
                video::Codec::H265
            },
            rotation: video::Rotation::from_bits(rng.byte()),
            ten_bit: rng.next() & 1 == 0,
            locked: rng.next() & 1 == 0,
            announced: rng.next() & 1 == 0,
            metadata: rng.next() & 1 == 0,
        };
        video::encode(&mut buf, &header).expect("encode");
        assert_eq!(video::parse(&buf).expect("parse"), header, "case {case}");

        // The metadata message the same header would open.
        let metadata = video::KeyframeMetadata {
            rebuilt: rng.next() & 1 == 0,
            keyframe: rng.next() & 1 == 0,
            ten_bit: rng.next() & 1 == 0,
            chroma_444: rng.next() & 1 == 0,
            rotation: video::Rotation::from_bits(rng.byte()),
        };
        video::encode_metadata(&mut buf, &header, &metadata).expect("encode metadata");
        let opened = video::parse(&buf).expect("parse metadata header");
        assert!(opened.metadata && opened.announced, "case {case}");
        assert_eq!(
            video::parse_metadata(&buf).expect("parse metadata"),
            metadata,
            "case {case}"
        );
    }
}

/// Fragmentation and reassembly are inverse across every size and capacity.
#[test]
fn messages_survive_fragmentation_and_reassembly() {
    let mut rng = Rng(0xA5A5_5A5A_1111_2222);
    const SLOT: usize = 300;
    const SLOTS: usize = 128;

    let mut payload = vec![0u8; 8192];
    let mut bodies = vec![0u8; SLOT * SLOTS];
    let mut meta = vec![SlotMeta::default(); SLOTS];
    let mut ring = RecvRing::new(&mut bodies, &mut meta, SLOT).unwrap();
    let mut fragment = vec![0u8; SLOT];
    let mut out = vec![0u8; 8192];

    let mut seq = 0u32;
    // Fewer cases: each one moves kilobytes through a real ring.
    for case in 0..(CASES / 10) {
        let len = rng.below(4000) as usize;
        let payload = &mut payload[..len];
        rng.fill(payload);
        let message = Message::new(&[], payload).expect("message");
        assert_eq!(
            message.fragment_count(SLOT),
            message::fragment_count(len as u32, SLOT),
            "case {case}: fragment count disagrees with the free function"
        );

        let mut index = 0;
        while let Some(result) = message.fragment(index, SLOT, &mut fragment) {
            let info = result.expect("fragment");
            ring.store(seq, &fragment[..info.len]);
            seq = seq.wrapping_add(1);
            index += 1;
        }
        assert_eq!(index, message.fragment_count(SLOT), "case {case}");

        let got = ring.take_message(&mut out).expect("complete").expect("ok");
        assert_eq!(got, len, "case {case}: length changed");
        assert_eq!(&out[..len], &payload[..], "case {case}: content changed");
    }
}

/// Sealing then opening is the identity for every plaintext length, on both
/// ciphers, with a counter that spans the range.
#[test]
fn records_round_trip_on_both_ciphers() {
    let mut rng = Rng(0xC0FF_EE00_3333_4444);
    let mut plaintext = [0u8; 1971];
    let mut wire = [0u8; 2000];
    let mut out = [0u8; 2000];

    for (label, key) in [("aes128", &[0x11u8; 16][..]), ("aes256", &[0x22u8; 32][..])] {
        let envelope = Envelope::from_key(key).expect("key");
        for case in 0..(CASES / 4) {
            let len = rng.below(1972) as usize;
            let plaintext = &mut plaintext[..len];
            rng.fill(plaintext);
            // The sealable domain: the counter's usable space is 48 bits, and
            // the sealer refuses anything past it rather than sending it.
            let counter = rng.next() & ((1 << 48) - 1);

            let written = envelope.seal(counter, plaintext, &mut wire).expect("seal");
            let opened = envelope.open(&wire[..written], &mut out).expect("open");
            assert_eq!(opened.counter, counter, "{label} case {case}");
            assert_eq!(opened.cleartext, &plaintext[..], "{label} case {case}");
        }
    }
}

/// The checksum a wireless pad report carries: the direction's seed, then
/// everything before the last four bytes, little endian at the end. Built
/// here independently of the module under test.
fn seal_wireless(seed: u8, report: &mut [u8]) {
    let at = report.len() - 4;
    let mut crc = crc32::Crc32::new();
    crc.update(&[seed]);
    crc.update(&report[..at]);
    report[at..].copy_from_slice(&crc.finish().to_le_bytes());
}

/// A controller's reports round-trip through the wire in both directions and
/// both framings, for every product, every feature report and every kind,
/// over random content: the DualShock 4's input body loses its identifier on
/// the wire and gets it back; a wireless report normalises to the USB one it
/// was framed from; an output report framed for a wireless pad carries the
/// USB content where the device reads it, under a checksum that verifies.
#[test]
fn pad_reports_round_trip_in_both_directions_and_framings() {
    let mut rng = Rng(0xD5_4D54_0000_0031);
    let mut buf = [0u8; 160];
    let mut usb = [0u8; pad::INPUT_LEN];
    let mut framed = [0u8; pad::REPORT_MAX];
    let mut out = [0u8; pad::INPUT_LEN];
    let mut feature_out = [0u8; pad::FEATURE_MAX];

    for case in 0..CASES {
        let product = if rng.below(2) == 0 {
            Product::DualShock4
        } else {
            Product::DualSense
        };
        let id = rng.next() as u32;

        // Input: a random report in the USB form.
        rng.fill(&mut usb);
        usb[0] = pad::USB_INPUT_ID;
        let body: &[u8] = match product {
            Product::DualShock4 => pad::ds4_body(&usb),
            Product::DualSense => &usb,
            _ => unreachable!(),
        };
        let n = pad::encode_report(&mut buf, id, product, false, body).expect("encode");
        let message = control::parse(&buf[..n]).expect("parse");
        assert_eq!(message.a1, id, "case {case}");
        assert_eq!(
            pad::parse_report(&message),
            Some(Inbound::Input {
                product,
                report: usb
            }),
            "case {case}: input"
        );
        // Feature: a random body under one of the two identifiers and lengths.
        let feature = if rng.below(2) == 0 {
            Feature::Calibration
        } else {
            Feature::Firmware
        };
        let len = feature.len(product);
        rng.fill(&mut feature_out[..len]);
        feature_out[0] = feature.id(product);
        let n =
            pad::encode_report(&mut buf, id, product, true, &feature_out[..len]).expect("encode");
        let message = control::parse(&buf[..n]).expect("parse");
        assert_eq!(
            message.a2 & pad::FEATURE_BIT,
            pad::FEATURE_BIT,
            "case {case}"
        );
        assert_eq!(
            pad::parse_report(&message),
            Some(Inbound::Feature {
                product,
                feature,
                report: &feature_out[..len]
            }),
            "case {case}: feature"
        );

        // Output: a random report of any length under either kind.
        let kind = if rng.below(2) == 0 {
            OutputKind::Output
        } else {
            OutputKind::Feature
        };
        let len = 1 + rng.below(pad::REPORT_MAX as u32) as usize;
        rng.fill(&mut framed[..len]);
        let n = pad::encode_output(&mut buf, id, kind, &framed[..len]).expect("encode");
        let message = control::parse(&buf[..n]).expect("parse");
        assert_eq!(
            pad::parse_output(&message),
            Some(Output {
                pad: id,
                kind,
                report: &framed[..len]
            }),
            "case {case}: output"
        );

        // Wireless input: framed as the pad frames it, normalised back.
        let mut wireless = [0u8; pad::BT_INPUT_LEN];
        rng.fill(&mut wireless);
        match product {
            Product::DualShock4 => {
                wireless[0] = 0x11;
                wireless[3..63].copy_from_slice(&usb[1..61]);
            }
            Product::DualSense => {
                wireless[0] = 0x31;
                wireless[2..65].copy_from_slice(&usb[1..]);
            }
            _ => unreachable!(),
        }
        seal_wireless(0xA1, &mut wireless);
        assert_eq!(
            pad::normalize_input(product, &wireless, &mut out).expect("normalise"),
            Transport::Bluetooth,
            "case {case}"
        );
        match product {
            Product::DualShock4 => {
                assert_eq!(&out[..61], &usb[..61], "case {case}: ds4 wireless");
                assert_eq!(&out[61..], &[0, 0, 0], "case {case}: ds4 wireless tail");
            }
            Product::DualSense => assert_eq!(out, usb, "case {case}: ds5 wireless"),
            _ => unreachable!(),
        }
        assert_eq!(
            pad::normalize_input(product, &usb, &mut out).expect("normalise"),
            Transport::Usb
        );
        assert_eq!(out, usb, "case {case}: usb passes through");

        // Wireless output: the USB content where the device reads it, sealed.
        let olen = product.output_len();
        rng.fill(&mut usb[..olen]);
        usb[0] = product.output_id();
        let seq = rng.below(16) as u8;
        let n = pad::frame_output(
            product,
            Transport::Bluetooth,
            seq,
            &usb[..olen],
            &mut framed,
        )
        .expect("frame");
        assert_eq!(n, pad::BT_OUTPUT_LEN, "case {case}");
        assert_eq!(
            &framed[3..3 + olen - 1],
            &usb[1..olen],
            "case {case}: content"
        );
        let mut check = framed;
        seal_wireless(0xA2, &mut check[..n]);
        assert_eq!(&check[..n], &framed[..n], "case {case}: checksum");
        let n = pad::frame_output(product, Transport::Usb, seq, &usb[..olen], &mut framed)
            .expect("frame");
        assert_eq!(
            &framed[..n],
            &usb[..olen],
            "case {case}: usb output passes through"
        );
    }
}
