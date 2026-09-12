//! Phase 13: the browser transport under the simulator.
//!
//! Two endpoints, one in each role, punch through the connectivity engine
//! exactly as the native pair does, then handshake and associate over a
//! path that loses, reorders and duplicates datagrams. Everything a shell
//! would do is done here against injected time, so a whole handshake and a
//! transfer under loss run in milliseconds and replay from a seed.
//!
//! What the native recovery test proves about the rings, this proves about
//! the pipe: every message arrives, whole and in order, and the figures the
//! congestion controller reads move the way they do natively when the path
//! polices.

// Fixtures build bytes from loop counters and percentiles from lengths; the
// truncating casts are the obvious ones.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lowlat_core::conn::{self, Conn, Credentials};
use lowlat_core::endpoint::{Endpoint, Fault, Media};
use lowlat_core::session::Health;
use lowlat_crypto::cert::{Certificate, FINGERPRINT_LEN};
use lowlat_net::web::{Role, WebSession};
use lowlat_sim::{HostId, Link, Sim};

const LEFT: (&str, &str) = ("aaaaaaaa", "passwordforaaaaaaaaaaaaa");
const RIGHT: (&str, &str) = ("bbbbbbbb", "passwordforbbbbbbbbbbbbb");

/// How often the loop wakes, in simulated milliseconds.
const TICK_MS: f64 = 5.0;

fn addr(last: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
}

struct Side {
    host: HostId,
    addr: SocketAddr,
    endpoint: Endpoint<'static, WebSession>,
}

/// Both ends, each with its own identity and the other's digest, candidates
/// exchanged, ready to punch at time zero.
fn pair(sim: &mut Sim, expect_left: Option<[u8; FINGERPRINT_LEN]>) -> (Side, Side) {
    let ours = Certificate::generate().unwrap();
    let theirs = Certificate::generate().unwrap();
    let left = side(
        sim,
        10,
        LEFT,
        RIGHT,
        0xA1,
        Role::Client,
        expect_left.unwrap_or(*theirs.fingerprint()),
        &ours,
    );
    let right = side(
        sim,
        20,
        RIGHT,
        LEFT,
        0xB2,
        Role::Server,
        *ours.fingerprint(),
        &theirs,
    );
    (left, right)
}

#[allow(clippy::too_many_arguments)]
fn side(
    sim: &mut Sim,
    last: u8,
    ours: (&'static str, &'static str),
    theirs: (&'static str, &'static str),
    seed: u8,
    role: Role,
    expect: [u8; FINGERPRINT_LEN],
    identity: &Certificate,
) -> Side {
    let addr = addr(last);
    let host = sim.add_host(addr, &[]);
    let conn = Conn::new(
        Credentials {
            local_ufrag: ours.0,
            local_pwd: ours.1,
            remote_ufrag: theirs.0,
            remote_pwd: theirs.1,
        },
        [seed; 16],
        0.0,
    );
    let session = WebSession::new(role, Some(expect), identity, 1, 0.0).unwrap();
    Side {
        host,
        addr,
        endpoint: Endpoint::new(conn, session),
    }
}

fn connect_candidates(left: &mut Side, right: &mut Side) {
    left.endpoint
        .conn()
        .add_candidate(right.addr, conn::Kind::Reflexive)
        .unwrap();
    right
        .endpoint
        .conn()
        .add_candidate(left.addr, conn::Kind::Reflexive)
        .unwrap();
    left.endpoint.conn().set_peer_ready();
    right.endpoint.conn().set_peer_ready();
}

/// One tick: drain both ends into the path, deliver what is due, advance.
fn tick(sim: &mut Sim, left: &mut Side, right: &mut Side) {
    let now = sim.now_ms();
    let mut wire = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
    for side in [&mut *left, &mut *right] {
        while let Some(result) = side.endpoint.get_output(now, &mut wire) {
            let egress = result.expect("a malformed datagram was emitted");
            sim.send(side.host, egress.to, 64, &wire[..egress.len]);
        }
    }
    while let Some(arrival) = sim.next_arrival() {
        let side = if arrival.host == left.host {
            &mut *left
        } else {
            &mut *right
        };
        // A datagram the path delivered may still be refused: a record that
        // arrives before the layer exists, or after it has closed.
        let _ = side
            .endpoint
            .process_input(&arrival.bytes, arrival.from, None, now, &mut scratch);
    }
    sim.advance_ms(TICK_MS);
    left.endpoint.poll(sim.now_ms());
    right.endpoint.poll(sim.now_ms());
}

fn run_until(
    sim: &mut Sim,
    left: &mut Side,
    right: &mut Side,
    budget_ms: f64,
    mut done: impl FnMut(&mut Side, &mut Side) -> bool,
) -> bool {
    let started = sim.now_ms();
    while sim.now_ms() - started < budget_ms {
        if done(left, right) {
            return true;
        }
        tick(sim, left, right);
    }
    done(left, right)
}

fn both_up(left: &mut Side, right: &mut Side) -> bool {
    left.endpoint.session().is_up() && right.endpoint.session().is_up()
}

fn take(side: &mut Side, channel: u8) -> Option<Vec<u8>> {
    let mut out = vec![0u8; 1 << 20];
    let len = side
        .endpoint
        .session()
        .take_message(channel, &mut out)?
        .unwrap();
    out.truncate(len);
    Some(out)
}

fn clean() -> Link {
    Link {
        one_way_ms: 20.0,
        ..Link::default()
    }
}

/// Loss, duplication and reordering, with next to no jitter. **Jitter is
/// the one condition this pipe cannot take**: five milliseconds of it,
/// uniform per datagram, reorders neighbours constantly, every reordering
/// is reported as a gap, and the association answers gaps by halving its
/// window -- a 760 KiB transfer that takes 475 ms on a clean link took
/// 6.2 s with jitter alone. That is the pipe's nature, and a real path
/// rarely reorders neighbours; the figure is kept so nobody re-measures it.
fn lossy() -> Link {
    Link {
        one_way_ms: 20.0,
        jitter_ms: 0.2,
        loss: 0.01,
        duplicate: 0.01,
        reorder: 0.05,
        reorder_ms: 15.0,
        ..Link::default()
    }
}

#[test]
fn a_web_pair_punches_handshakes_and_associates() {
    let mut sim = Sim::new(0x13).with_link(clean());
    let (mut left, mut right) = pair(&mut sim, None);
    connect_candidates(&mut left, &mut right);

    assert!(
        run_until(&mut sim, &mut left, &mut right, 5_000.0, |l, r| {
            l.endpoint.path().is_some() && r.endpoint.path().is_some()
        }),
        "no path"
    );
    assert!(
        run_until(&mut sim, &mut left, &mut right, 5_000.0, both_up),
        "no association: {:?} / {:?}",
        left.endpoint.session(),
        right.endpoint.session()
    );
    assert_eq!(left.endpoint.fault(), None);
    assert_eq!(right.endpoint.fault(), None);
    println!(
        "web_pair: path and association in {:.0} ms simulated",
        sim.now_ms()
    );
}

#[test]
fn a_keyframe_of_600_kib_crosses_whole_and_in_order() {
    let mut sim = Sim::new(0x600).with_link(lossy());
    let (mut left, mut right) = pair(&mut sim, None);
    connect_candidates(&mut left, &mut right);
    assert!(run_until(
        &mut sim, &mut left, &mut right, 10_000.0, both_up
    ));

    let keyframe: Vec<u8> = (0..600 * 1024).map(|i| (i % 253) as u8).collect();
    let deltas: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 20_000]).collect();
    left.endpoint
        .session()
        .send_message(1, b"VIDEO-HDR!", &keyframe)
        .unwrap();
    for delta in &deltas {
        left.endpoint
            .session()
            .send_message(1, b"VIDEO-HDR!", delta)
            .unwrap();
    }

    let mut got = Vec::new();
    assert!(
        run_until(&mut sim, &mut left, &mut right, 60_000.0, |_, r| {
            while let Some(message) = take(r, 1) {
                got.push(message);
            }
            got.len() == 9
        }),
        "only {} of 9 messages arrived",
        got.len()
    );
    assert_eq!(got[0], keyframe);
    for (index, delta) in deltas.iter().enumerate() {
        assert_eq!(&got[index + 1], delta, "delta {index}");
    }
    let drops = sim.take_drops().len();
    let (_, srtt, _, rtx, _) = left.endpoint.session().figures(1).unwrap();
    // **Seven seconds for what a clean link carries in half a second.** The
    // association answers every loss and every reordering by halving its
    // window and then growing it one packet per round trip, so what this
    // pipe carries on a lossy path is bounded the way any fair stream is:
    // about 1.2 packets per round trip over the square root of the loss
    // rate, which at this path's 60 ms and one percent is two megabits.
    // The native transport sends at the rate the host chooses and repairs
    // the gaps; this one cannot, and the rate controller has to follow it
    // down. Recorded here so nobody re-measures it.
    println!(
        "web_pair: 600 KiB keyframe and eight deltas in {:.0} ms simulated, {drops} datagrams \
         lost, {rtx} retransmitted, srtt {srtt:.0} ms",
        sim.now_ms()
    );
    assert!(drops > 0, "the path lost nothing, so nothing was recovered");
}

#[test]
fn a_keyframe_on_a_clean_link_for_the_baseline() {
    let mut sim = Sim::new(0x601).with_link(clean());
    let (mut left, mut right) = pair(&mut sim, None);
    connect_candidates(&mut left, &mut right);
    assert!(run_until(
        &mut sim, &mut left, &mut right, 10_000.0, both_up
    ));
    let started = sim.now_ms();
    let keyframe: Vec<u8> = (0..600 * 1024).map(|i| (i % 253) as u8).collect();
    left.endpoint
        .session()
        .send_message(1, b"VIDEO-HDR!", &keyframe)
        .unwrap();
    assert!(run_until(
        &mut sim,
        &mut left,
        &mut right,
        60_000.0,
        |_, r| take(r, 1).is_some()
    ));
    println!(
        "web_pair: 600 KiB keyframe on a clean 20 ms link in {:.0} ms simulated",
        sim.now_ms() - started
    );
}

#[test]
fn every_message_arrives_in_order_under_loss_and_reorder() {
    const TOTAL: u32 = 2_000;
    let mut sim = Sim::new(0x2000).with_link(lossy());
    let (mut left, mut right) = pair(&mut sim, None);
    connect_candidates(&mut left, &mut right);
    assert!(run_until(
        &mut sim, &mut left, &mut right, 10_000.0, both_up
    ));

    let mut queued = 0u32;
    let mut received = 0u32;
    let ok = run_until(&mut sim, &mut left, &mut right, 300_000.0, |l, r| {
        while queued < TOTAL {
            let mut payload = [0u8; 200];
            payload[..4].copy_from_slice(&queued.to_be_bytes());
            if l.endpoint
                .session()
                .send_message(2, b"AUDIO", &payload)
                .is_err()
            {
                break;
            }
            queued += 1;
        }
        while let Some(message) = take(r, 2) {
            let index = u32::from_be_bytes(message[..4].try_into().unwrap());
            assert_eq!(index, received, "arrived out of order");
            assert_eq!(message.len(), 200);
            received += 1;
        }
        received == TOTAL
    });
    assert!(ok, "only {received} of {TOTAL} arrived");
    assert_eq!(right.endpoint.session().recv_cumulative(2), Some(TOTAL));
}

#[test]
fn a_wrong_peer_fingerprint_is_a_handshake_fault() {
    let mut sim = Sim::new(0xBAD).with_link(clean());
    let (mut left, mut right) = pair(&mut sim, Some([0xEE; FINGERPRINT_LEN]));
    connect_candidates(&mut left, &mut right);
    assert!(run_until(
        &mut sim,
        &mut left,
        &mut right,
        10_000.0,
        |l, _| { l.endpoint.fault().is_some() }
    ));
    assert_eq!(left.endpoint.fault(), Some(Fault::Handshake));
    assert!(!left.endpoint.session().is_up());
    assert_eq!(left.endpoint.health(sim.now_ms()), Health::Dead);
}

#[test]
fn a_close_notify_is_dead_without_a_fault() {
    let mut sim = Sim::new(0xC105E).with_link(clean());
    let (mut left, mut right) = pair(&mut sim, None);
    connect_candidates(&mut left, &mut right);
    assert!(run_until(
        &mut sim, &mut left, &mut right, 10_000.0, both_up
    ));
    left.endpoint.session().close();
    assert!(run_until(
        &mut sim,
        &mut left,
        &mut right,
        5_000.0,
        |_, r| { r.endpoint.health(0.0) == Health::Dead }
    ));
    assert_eq!(right.endpoint.fault(), None);
}

/// The figures the controller steers by rise when the path cannot carry
/// what is offered: the window past the floor, and a stale share within it.
#[test]
fn pressure_grows_when_the_link_polices() {
    let mut sim = Sim::new(0x9011).with_link(Link {
        one_way_ms: 30.0,
        capacity_bytes_per_ms: 300.0,
        ..Link::default()
    });
    let (mut left, mut right) = pair(&mut sim, None);
    connect_candidates(&mut left, &mut right);
    assert!(run_until(
        &mut sim, &mut left, &mut right, 10_000.0, both_up
    ));

    let mut peak_window = 0u32;
    let mut peak_stale = 0u32;
    let mut frames = 0u32;
    let mut tick_count = 0u32;
    run_until(&mut sim, &mut left, &mut right, 3_000.0, |l, r| {
        // A 30 KiB picture every 16 ms onto a path that carries 300 bytes a
        // millisecond: what is offered outruns what is carried by six to one.
        tick_count += 1;
        if tick_count % 3 == 0 {
            let _ = l
                .endpoint
                .session()
                .send_message(1, b"VIDEO-HDR!", &vec![0x5A; 30 * 1024]);
            frames += 1;
        }
        while take(r, 1).is_some() {}
        let pressure = l.endpoint.session().send_pressure(1).unwrap();
        peak_window = peak_window.max(pressure.window);
        peak_stale = peak_stale.max(pressure.stale);
        false
    });
    println!(
        "web_pair: {frames} frames offered, window peaked at {peak_window} fragments with {peak_stale} stale"
    );
    assert!(
        peak_window > lowlat_core::congestion::WINDOW_FLOOR,
        "the window never passed the floor: {peak_window}"
    );
    assert!(peak_stale > 0, "nothing went stale on a policed path");
    assert!(
        left.endpoint.session().srtt_ms() > 0.0,
        "no round trip measured"
    );
}

/// Both ends beginning the association at once still associate.
#[test]
fn the_init_collision_still_associates() {
    let mut sim = Sim::new(0xC011).with_link(clean());
    let ours = Certificate::generate().unwrap();
    let theirs = Certificate::generate().unwrap();
    let mut left = side(
        &mut sim,
        10,
        LEFT,
        RIGHT,
        0xA1,
        Role::Client,
        *theirs.fingerprint(),
        &ours,
    );
    // The right side is the DTLS server but begins the association the
    // moment its record layer is up, which a peer that races us would do.
    let mut right = side(
        &mut sim,
        20,
        RIGHT,
        LEFT,
        0xB2,
        Role::Server,
        *ours.fingerprint(),
        &theirs,
    );
    connect_candidates(&mut left, &mut right);
    let mut begun = false;
    assert!(run_until(
        &mut sim,
        &mut left,
        &mut right,
        10_000.0,
        |l, r| {
            if !begun && r.endpoint.session().is_up() {
                r.endpoint.session().begin_association();
                begun = true;
            }
            both_up(l, r)
        }
    ));
    left.endpoint
        .session()
        .send_message(0, b"HDR-13-BYTES!", b"")
        .unwrap();
    right
        .endpoint
        .session()
        .send_message(0, b"HDR-13-BYTES!", b"x")
        .unwrap();
    let mut got = (false, false);
    assert!(
        run_until(&mut sim, &mut left, &mut right, 10_000.0, |l, r| {
            got.0 |= take(l, 0).is_some();
            got.1 |= take(r, 0).is_some();
            got.0 && got.1
        }),
        "messages did not cross both ways: {got:?}"
    );
}
