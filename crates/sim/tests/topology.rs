//! Phase 2 gate 1: the topology matrix, each case with its expected outcome.
//!
//! Half of this matrix is expected to fail. A suite that only asserted "a path
//! was found" would pass while reporting failure for everything, so every case
//! states which outcome it expects, and the two disabled-behaviour cases at the
//! bottom exist to show the harness can produce the other answer.
//!
//! Both sides run the same engine and probe simultaneously, which is the normal
//! case rather than an exception.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lowlat_core::conn::{Conn, Credentials, Failure, PROBE_TTL, PUNCH_WINDOW_MS, State, Ttl};
use lowlat_sim::{HostId, Nat, Sim};

const A_UFRAG: &str = "aaaa";
const A_PWD: &str = "passwordforaaaa";
const B_UFRAG: &str = "bbbb";
const B_PWD: &str = "passwordforbbbb";

/// A public reflexive server, used only to learn a mapping.
const SERVER: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 50)), 3478);

/// Ordinary TTL for anything that is not a mapping probe.
const NORMAL_TTL: u8 = 64;

/// How often the loop wakes. Fine enough that the 10 ms send pacing and the
/// 500 ms check cadence both resolve.
const TICK_MS: f64 = 5.0;

fn public_ip(last: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(203, 0, 113, last))
}

fn private(last: u8, port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, last)), port)
}

fn credentials(
    ours: (&'static str, &'static str),
    theirs: (&'static str, &'static str),
) -> Credentials<'static> {
    Credentials {
        local_ufrag: ours.0,
        local_pwd: ours.1,
        remote_ufrag: theirs.0,
        remote_pwd: theirs.1,
    }
}

/// Drain everything an endpoint wants to send right now onto the network.
fn pump(sim: &mut Sim, conn: &mut Conn<'_>, host: HostId, now_ms: f64, buf: &mut [u8]) {
    while let Some(result) = conn.get_output(now_ms, buf) {
        let egress = result.expect("the engine emitted a malformed datagram");
        let ttl = match egress.ttl {
            Ttl::Probe => PROBE_TTL,
            _ => NORMAL_TTL,
        };
        sim.send(host, egress.to, ttl, &buf[..egress.len]);
    }
}

/// Run one attempt to completion and report where both sides ended up.
///
/// `build` installs the topology and returns the two endpoints.
fn punch(build: impl FnOnce(&mut Sim) -> (HostId, HostId)) -> (State, State) {
    let mut sim = Sim::new(0xB01D_FACE);
    let (a_host, b_host) = build(&mut sim);
    sim.add_host(SERVER, &[]);

    // Each side learns one candidate the way it really would, by asking a
    // server. Under symmetric translation this is deliberately not the address
    // its own checks will leave from.
    let a_candidate = sim.reflexive(a_host, SERVER);
    let b_candidate = sim.reflexive(b_host, SERVER);

    let mut a = Conn::new(
        credentials((A_UFRAG, A_PWD), (B_UFRAG, B_PWD)),
        [0xA1; 16],
        0.0,
    );
    let mut b = Conn::new(
        credentials((B_UFRAG, B_PWD), (A_UFRAG, A_PWD)),
        [0xB2; 16],
        0.0,
    );
    a.add_candidate(b_candidate).expect("candidate refused");
    b.add_candidate(a_candidate).expect("candidate refused");

    let mut buf = [0u8; 256];
    while sim.now_ms() <= PUNCH_WINDOW_MS {
        let now = sim.now_ms();
        pump(&mut sim, &mut a, a_host, now, &mut buf);
        pump(&mut sim, &mut b, b_host, now, &mut buf);

        while let Some(arrival) = sim.next_arrival() {
            let conn = if arrival.host == a_host {
                &mut a
            } else {
                &mut b
            };
            // A datagram that fails authentication is dropped, not fatal.
            let _ = conn.process_input(&arrival.bytes, arrival.from);
        }

        if matches!(a.state(), State::Established(_)) && matches!(b.state(), State::Established(_))
        {
            break;
        }

        sim.advance_ms(TICK_MS);
        a.poll(sim.now_ms());
        b.poll(sim.now_ms());
    }

    (a.state(), b.state())
}

fn assert_established(outcome: (State, State)) {
    assert!(
        matches!(outcome.0, State::Established(_)),
        "the initiating side found no path: {:?}",
        outcome.0
    );
    assert!(
        matches!(outcome.1, State::Established(_)),
        "the answering side found no path: {:?}",
        outcome.1
    );
}

/// Both sides must fail, and specifically by exhausting the window with checks
/// unanswered. Accepting any failure would also accept an attempt that never
/// had a candidate to check, which would pass this matrix while testing
/// nothing.
fn assert_timed_out(outcome: (State, State)) {
    for state in [outcome.0, outcome.1] {
        assert_eq!(
            state,
            State::Failed(Failure::ProbeTimeout),
            "expected checks to go unanswered, got {state:?}"
        );
    }
}

fn pair(sim: &mut Sim, left: Nat, right: Nat) -> (HostId, HostId) {
    let left = sim.add_nat(left);
    let right = sim.add_nat(right);
    (
        sim.add_host(private(10, 5000), &[left]),
        sim.add_host(private(20, 6000), &[right]),
    )
}

#[test]
fn full_cone_establishes() {
    assert_established(punch(|sim| {
        pair(
            sim,
            Nat::full_cone(public_ip(1)),
            Nat::full_cone(public_ip(2)),
        )
    }));
}

#[test]
fn restricted_cone_establishes() {
    assert_established(punch(|sim| {
        pair(
            sim,
            Nat::restricted_cone(public_ip(1)),
            Nat::restricted_cone(public_ip(2)),
        )
    }));
}

#[test]
fn port_restricted_establishes() {
    assert_established(punch(|sim| {
        pair(
            sim,
            Nat::port_restricted(public_ip(1)),
            Nat::port_restricted(public_ip(2)),
        )
    }));
}

/// The case the relay exists for. The address each side advertised was created
/// toward the reflexive server, and the mapping toward the peer is a different
/// port, so every check reaches a port nobody is listening on.
#[test]
fn symmetric_times_out() {
    assert_timed_out(punch(|sim| {
        pair(
            sim,
            Nat::symmetric(public_ip(1)),
            Nat::symmetric(public_ip(2)),
        )
    }));
}

/// One symmetric side is enough to defeat the punch. Worth its own case,
/// because a matrix that only tested the symmetric pair would miss it.
#[test]
fn one_symmetric_side_times_out() {
    assert_timed_out(punch(|sim| {
        pair(
            sim,
            Nat::port_restricted(public_ip(1)),
            Nat::symmetric(public_ip(2)),
        )
    }));
}

/// A symmetric side and a peer that filters nothing. The advertised address is
/// useless in one direction only: our checks reach the peer, because it admits
/// anyone, but its checks go to the mapping we made toward the reflexive server
/// and land on a port nothing is listening on.
///
/// The peer can only get there by using the address our check actually arrived
/// from. This is the case that requires it, and the pairing a real wide-area run
/// produced on the first attempt.
#[test]
fn a_symmetric_side_is_reached_at_the_address_its_checks_come_from() {
    assert_established(punch(|sim| {
        pair(
            sim,
            Nat::symmetric(public_ip(1)),
            Nat::full_cone(public_ip(2)),
        )
    }));
}

/// Two layers of translation do not break a punch on their own. What matters is
/// the mapping behaviour, not the number of layers, and a carrier translator
/// that keeps mappings endpoint independent is punchable.
#[test]
fn carrier_grade_with_endpoint_independent_mapping_establishes() {
    assert_established(punch(|sim| {
        let cpe_a = sim.add_nat(Nat::port_restricted(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 5,
        ))));
        let cgn_a = sim.add_nat(Nat::port_restricted(public_ip(1)));
        let cpe_b = sim.add_nat(Nat::port_restricted(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 6,
        ))));
        let cgn_b = sim.add_nat(Nat::port_restricted(public_ip(2)));
        (
            sim.add_host(private(10, 5000), &[cpe_a, cgn_a]),
            sim.add_host(private(20, 6000), &[cpe_b, cgn_b]),
        )
    }));
}

/// And the same topology with a symmetric carrier translator does break it, so
/// the previous case is passing on the mapping behaviour rather than on the
/// harness being unable to fail.
#[test]
fn carrier_grade_with_symmetric_mapping_times_out() {
    assert_timed_out(punch(|sim| {
        let cpe_a = sim.add_nat(Nat::port_restricted(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 5,
        ))));
        let cgn_a = sim.add_nat(Nat::symmetric(public_ip(1)));
        let cpe_b = sim.add_nat(Nat::port_restricted(IpAddr::V4(Ipv4Addr::new(
            100, 64, 0, 6,
        ))));
        let cgn_b = sim.add_nat(Nat::symmetric(public_ip(2)));
        (
            sim.add_host(private(10, 5000), &[cpe_a, cgn_a]),
            sim.add_host(private(20, 6000), &[cpe_b, cgn_b]),
        )
    }));
}

/// Both endpoints behind one translator, reaching each other by its external
/// address.
#[test]
fn hairpin_establishes() {
    assert_established(punch(|sim| {
        let nat = sim.add_nat(Nat::port_restricted(public_ip(1)));
        (
            sim.add_host(private(10, 5000), &[nat]),
            sim.add_host(private(11, 6000), &[nat]),
        )
    }));
}

/// The same topology on a translator that will not loop back. This is the
/// proof that the hairpin case above is testing hairpin support rather than
/// passing for some unrelated reason.
#[test]
fn hairpin_disabled_times_out() {
    assert_timed_out(punch(|sim| {
        let nat = sim.add_nat(Nat::port_restricted(public_ip(1)).with_hairpin(false));
        (
            sim.add_host(private(10, 5000), &[nat]),
            sim.add_host(private(11, 6000), &[nat]),
        )
    }));
}
