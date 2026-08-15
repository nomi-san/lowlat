//! Phase 2 gate 5: recovery under loss and reordering.
//!
//! Ten thousand messages across a path that loses five percent of datagrams and
//! holds another five percent back behind later ones. Every message must arrive,
//! in order, and the run must finish inside a bounded amount of simulated time
//! rather than merely making progress.
//!
//! This drives the protocol core rather than the connectivity engine. It sits
//! here because the simulator is what makes it expressible: the condition is
//! reproducible from a seed, and fifteen minutes of adversarial path behaviour
//! runs in well under a second.
//!
//! The frame-level form of this property, a bounded freeze with no reference
//! chain broken, belongs to Gate A, where an encoder exists to produce frames.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::envelope::Envelope;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_sim::{Link, Sim};

const TOTAL: usize = 10_000;
const CHANNEL: u8 = 1;
const SLOT: usize = 64;
const SLOTS: usize = 1024;
const KEY: [u8; 32] = [0x3C; 32];

/// Ceiling on simulated time. Generous, but finite: a recovery path that makes
/// progress while never converging would otherwise pass by running forever.
const BUDGET_MS: f64 = 900_000.0;

/// How often the loop wakes, in simulated milliseconds.
const TICK_MS: f64 = 5.0;

fn addr(last: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
}

struct Arena {
    recv_bodies: Vec<u8>,
    recv_meta: Vec<SlotMeta>,
    send_bodies: Vec<u8>,
    send_meta: Vec<SendSlot>,
}

impl Arena {
    fn new() -> Self {
        Self {
            recv_bodies: vec![0u8; SLOT * SLOTS],
            recv_meta: vec![SlotMeta::default(); SLOTS],
            send_bodies: vec![0u8; SLOT * SLOTS],
            send_meta: vec![SendSlot::default(); SLOTS],
        }
    }
}

fn endpoint(arena: &mut Arena) -> Session<'_> {
    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    session
        .attach_recv(
            CHANNEL,
            RecvRing::new(&mut arena.recv_bodies, &mut arena.recv_meta, SLOT).unwrap(),
        )
        .unwrap();
    session
        .attach_send(
            CHANNEL,
            SendRing::new(&mut arena.send_bodies, &mut arena.send_meta, SLOT, CHANNEL).unwrap(),
        )
        .unwrap();
    session
}

/// Run the transfer and return how much simulated time it took.
fn transfer(seed: u64, link: Link) -> f64 {
    let mut sim = Sim::new(seed).with_link(link);
    let tx_host = sim.add_host(addr(10), &[]);
    let rx_host = sim.add_host(addr(20), &[]);

    let mut tx_arena = Arena::new();
    let mut rx_arena = Arena::new();
    let mut tx = endpoint(&mut tx_arena);
    let mut rx = endpoint(&mut rx_arena);

    let mut queued = 0usize;
    let mut received = 0usize;
    let mut wire = [0u8; 512];
    let mut scratch = [0u8; 512];
    let mut out = [0u8; 512];

    while received < TOTAL && sim.now_ms() < BUDGET_MS {
        let now = sim.now_ms();

        // Offer as much as the window will take. A refusal is backpressure,
        // not an error, so it simply waits for the next tick.
        while queued < TOTAL {
            let mut payload = [0u8; 12];
            let index = u32::try_from(queued).expect("index outside the transfer");
            payload[..4].copy_from_slice(&index.to_be_bytes());
            if tx.send_message(CHANNEL, &[], &payload).is_err() {
                break;
            }
            queued += 1;
        }

        while let Some(result) = tx.get_output(now, &mut wire) {
            let len = result.expect("sender emitted a malformed datagram");
            sim.send(tx_host, addr(20), 64, &wire[..len]);
        }
        while let Some(result) = rx.get_output(now, &mut wire) {
            let len = result.expect("receiver emitted a malformed datagram");
            sim.send(rx_host, addr(10), 64, &wire[..len]);
        }

        while let Some(arrival) = sim.next_arrival() {
            let session = if arrival.host == tx_host {
                &mut tx
            } else {
                &mut rx
            };
            // A datagram that fails to parse would be a defect, not a path
            // condition: the simulator corrupts nothing.
            session
                .process_input(&arrival.bytes, now, &mut scratch)
                .expect("a delivered datagram failed to parse");
        }

        // Everything complete on the channel, in the order it was sent.
        while let Some(result) = rx.take_message(CHANNEL, &mut out) {
            let len = result.expect("reassembly failed");
            let index = usize::try_from(u32::from_be_bytes(out[..4].try_into().unwrap()))
                .expect("index outside the transfer");
            assert_eq!(
                index, received,
                "message {received} arrived out of order as {index}"
            );
            assert_eq!(len, 12, "message {received} arrived with the wrong length");
            received += 1;
        }

        sim.advance_ms(TICK_MS);
        tx.poll(sim.now_ms());
        rx.poll(sim.now_ms());
    }

    assert_eq!(
        received, TOTAL,
        "only {received} of {TOTAL} messages arrived within {BUDGET_MS} ms"
    );
    let drops = sim.take_drops().len();
    println!(
        "recovery: seed={seed:#x} loss={:.0}% reorder={:.0}% -> {TOTAL} messages in \
         {:.0} ms simulated, {drops} datagrams discarded by the path",
        link.loss * 100.0,
        link.reorder * 100.0,
        sim.now_ms()
    );
    sim.now_ms()
}

/// The gate. Five percent loss and five percent reordering, both directions,
/// which means acknowledgements are lost and delayed as well as data.
#[test]
fn ten_thousand_messages_survive_loss_and_reordering() {
    let elapsed = transfer(
        0x5EED,
        Link {
            loss: 0.05,
            reorder: 0.05,
            reorder_ms: 40.0,
            jitter_ms: 3.0,
            ..Link::default()
        },
    );
    assert!(
        elapsed < BUDGET_MS,
        "recovery did not converge: {elapsed} ms"
    );
}

/// A clean path, as the control. If the lossy run took a similar time, the
/// conditions above were not actually reaching the transport.
#[test]
fn a_clean_path_is_faster_than_a_lossy_one() {
    let clean = transfer(0x5EED, Link::default());
    let lossy = transfer(
        0x5EED,
        Link {
            loss: 0.05,
            reorder: 0.05,
            reorder_ms: 40.0,
            ..Link::default()
        },
    );
    assert!(
        lossy > clean,
        "loss and reordering cost nothing, so the conditions did not apply: \
         clean {clean} ms, lossy {lossy} ms"
    );
}

/// Heavier than the gate, to show the recovery path is not tuned to one figure.
#[test]
fn a_severely_degraded_path_still_converges() {
    transfer(
        0xD1CE,
        Link {
            loss: 0.20,
            reorder: 0.10,
            reorder_ms: 80.0,
            duplicate: 0.05,
            jitter_ms: 10.0,
            ..Link::default()
        },
    );
}
