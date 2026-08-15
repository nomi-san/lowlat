//! Phase 1 gate 5: the data paths allocate nothing.
//!
//! The core is `no_std` without `alloc`, so it *cannot* allocate directly.
//! That is not the whole claim. This asserts the paths behave as promised when
//! linked into a program that does have an allocator, which is how they will
//! actually run, and it catches a dependency that allocates on our behalf.
//!
//! Setup is free to allocate. Only the region inside `assert_no_alloc` is
//! covered, which is exactly the receive, send, and reassembly hot paths.

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::envelope::Envelope;
use lowlat_core::message::Message;
use lowlat_core::packet::{self, Data};
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;

#[global_allocator]
static ALLOC: Counting = Counting;

const KEY: [u8; 32] = [0x5Au8; 32];
const SLOT: usize = 1500;
const SLOTS: usize = 64;
const CHANNEL: u8 = 1;

#[test]
fn envelope_seal_and_open_do_not_allocate() {
    let envelope = Envelope::from_key(&KEY).unwrap();
    let plaintext = [0xABu8; 1200];
    let mut wire = [0u8; 1400];
    let mut out = [0u8; 1400];

    alloc_counter::assert_no_alloc(|| {
        for counter in 0..64u64 {
            let n = envelope.seal(counter, &plaintext, &mut wire).unwrap();
            let opened = envelope.open(&wire[..n], &mut out).unwrap();
            std::hint::black_box(opened.counter);
        }
    });
}

#[test]
fn packet_parse_and_encode_do_not_allocate() {
    let body = [0x11u8; 1100];
    let data = Data {
        channel: CHANNEL,
        seq: 12345,
        last: true,
        body: &body,
    };
    let mut buf = [0u8; 1400];

    alloc_counter::assert_no_alloc(|| {
        for _ in 0..256 {
            let n = packet::encode_data(&mut buf, &data).unwrap();
            std::hint::black_box(packet::parse(&buf[..n]).unwrap());
        }
    });
}

#[test]
fn ring_store_and_reassembly_do_not_allocate() {
    let mut bodies = vec![0u8; SLOT * SLOTS];
    let mut meta = vec![SlotMeta::default(); SLOTS];
    let mut ring = RecvRing::new(&mut bodies, &mut meta, SLOT).unwrap();

    let payload = [0x7Fu8; 3000];
    let message = Message::new(&[], &payload).unwrap();
    let mut fragment = [0u8; SLOT];
    let mut out = vec![0u8; 8192];

    alloc_counter::assert_no_alloc(|| {
        let mut seq = 0u32;
        for _ in 0..16 {
            let mut index = 0;
            while let Some(result) = message.fragment(index, SLOT, &mut fragment) {
                let written = result.unwrap();
                ring.store(seq, &fragment[..written.len]);
                seq = seq.wrapping_add(1);
                index += 1;
            }
            let len = ring.take_message(&mut out).unwrap().unwrap();
            std::hint::black_box(len);
        }
    });
}

#[test]
fn send_ring_enqueue_and_drain_do_not_allocate() {
    let mut bodies = vec![0u8; SLOT * SLOTS];
    let mut meta = vec![SendSlot::default(); SLOTS];
    let mut ring = SendRing::new(&mut bodies, &mut meta, SLOT, CHANNEL).unwrap();
    let payload = [0x33u8; 2000];
    let mut out = [0u8; 1600];

    alloc_counter::assert_no_alloc(|| {
        let message = Message::new(&[], &payload).unwrap();
        ring.enqueue(&message).unwrap();
        ring.begin_pass();
        while let Some(result) = ring.poll_send(0.0, 10.0, 1, &mut out) {
            std::hint::black_box(result.unwrap());
        }
    });
}

/// The whole loop the shell will run, under the counter.
#[test]
fn a_session_round_does_not_allocate() {
    let mut recv_bodies = vec![0u8; SLOT * SLOTS];
    let mut recv_meta = vec![SlotMeta::default(); SLOTS];
    let mut send_bodies = vec![0u8; SLOT * SLOTS];
    let mut send_meta = vec![SendSlot::default(); SLOTS];

    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    session
        .attach_recv(
            CHANNEL,
            RecvRing::new(&mut recv_bodies, &mut recv_meta, SLOT).unwrap(),
        )
        .unwrap();
    session
        .attach_send(
            CHANNEL,
            SendRing::new(&mut send_bodies, &mut send_meta, SLOT, CHANNEL).unwrap(),
        )
        .unwrap();

    let payload = [0x42u8; 2500];
    let mut wire = [0u8; 1600];
    let mut scratch = [0u8; 1600];
    let mut message = vec![0u8; 8192];

    // Prime the paths once so any lazy one-time setup happens outside the
    // assertion rather than being blamed on the hot loop.
    session.send_message(CHANNEL, &[], &payload).unwrap();
    while let Some(result) = session.get_output(0.0, &mut wire) {
        result.unwrap();
    }

    alloc_counter::assert_no_alloc(|| {
        for round in 1..32u32 {
            let now = f64::from(round);
            session.send_message(CHANNEL, &[], &payload).unwrap();
            while let Some(result) = session.get_output(now, &mut wire) {
                let written = result.unwrap();
                session
                    .process_input(&wire[..written], now, &mut scratch)
                    .unwrap();
            }
            session.poll(now);
            while let Some(result) = session.take_message(CHANNEL, &mut message) {
                std::hint::black_box(result.unwrap());
            }
        }
    });
}
