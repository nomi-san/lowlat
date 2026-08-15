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
use lowlat_core::{control, video};

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
            rotation: video::Rotation::from_bits(rng.byte()),
            keyframe: rng.next() & 1 == 0,
            fullscreen: rng.next() & 1 == 0,
        };
        video::encode(&mut buf, &header).expect("encode");
        assert_eq!(video::parse(&buf).expect("parse"), header, "case {case}");
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
            let counter = rng.next();

            let written = envelope.seal(counter, plaintext, &mut wire).expect("seal");
            let opened = envelope.open(&wire[..written], &mut out).expect("open");
            assert_eq!(opened.counter, counter, "{label} case {case}");
            assert_eq!(opened.cleartext, &plaintext[..], "{label} case {case}");
        }
    }
}
