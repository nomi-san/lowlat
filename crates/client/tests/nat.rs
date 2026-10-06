//! The translation in front of a handle's port, probed on demand: what two
//! servers' answers say of it, a probe refused while the port is taken, and
//! an attempt that stops one and leaves what was known before.

mod support;

use std::net::Ipv4Addr;
use std::thread;
use std::time::{Duration, Instant};

use lowlat_client::config::{Backend, Decoding, Port};
use lowlat_client::nat::{Source, State};
use lowlat_client::{Client, Config, Error, Peer, Transport};
use lowlat_core::nat::Mapping;
use support::server;

/// A client with no decoder, on a port the system picks.
fn client() -> Client {
    Client::new(
        &Decoding {
            backend: Backend::None,
            ..Default::default()
        },
        Port::Any,
    )
    .expect("a client without a decoder")
}

/// Wait for `holds`, failing with `what` after a generous bound.
fn until(what: &str, mut holds: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !holds() {
        assert!(Instant::now() < end, "never: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

fn peer() -> Peer {
    let theirs = lowlat_crypto::credentials().expect("a peer's credentials");
    Peer {
        ufrag: theirs.ufrag,
        pwd: theirs.pwd,
        fingerprint: theirs.fingerprint,
        aes256: Some(theirs.aes256),
    }
}

/// Two servers at two addresses that each see the port moved by its own
/// distance: a port per destination, numbered 3, the first server's view
/// the public address; asked again, both moved alike, one port for all.
#[test]
fn a_probe_reads_the_translation_from_two_servers() {
    let mut one = client();
    assert_eq!(one.nat().state, State::None);

    let (a, first) = server(Ipv4Addr::LOCALHOST, None, 1, 1);
    let (b, second) = server(Ipv4Addr::new(127, 0, 0, 2), None, 2, 1);
    one.probe_nat(vec![a, b], Duration::from_secs(3))
        .expect("a probe");
    until("the probe's result", || one.nat().state == State::Done);
    first.join().unwrap();
    second.join().unwrap();
    let nat = one.nat();
    assert_eq!(nat.source, Source::Probe);
    assert_eq!(nat.mapping, Some(Mapping::Dependent));
    assert_eq!(nat.number, Some(3));
    assert_eq!((nat.asked, nat.answered), (2, 2));
    assert!(!nat.port_preserved);

    let (a, first) = server(Ipv4Addr::LOCALHOST, None, 7, 1);
    let (b, second) = server(Ipv4Addr::new(127, 0, 0, 2), None, 7, 1);
    one.probe_nat(vec![a, b], Duration::from_secs(3))
        .expect("a second probe once the first is done");
    until("the second result", || {
        one.nat().mapping == Some(Mapping::Independent)
    });
    first.join().unwrap();
    second.join().unwrap();
    assert_eq!(one.nat().number, Some(2));
}

/// A probe is one at a time, and none while an attempt holds the port; an
/// attempt begun during one stops it at once, and what was known before
/// stands.
#[test]
fn an_attempt_stops_a_probe_and_holds_the_port_against_another() {
    let mut one = client();
    let (silent, _never) = server(Ipv4Addr::LOCALHOST, None, 0, 0);
    one.probe_nat(vec![silent], Duration::from_secs(20))
        .expect("a probe");
    assert_eq!(one.nat().state, State::Probing);
    assert!(
        matches!(
            one.probe_nat(vec![silent], Duration::from_secs(1)),
            Err(Error::Busy)
        ),
        "a second probe ran beside the first"
    );

    one.new_attempt("a", Config::default(), Transport::Bud)
        .expect("credentials");
    let began = Instant::now();
    one.begin_p2p("a", &peer()).expect("the attempt began");
    assert!(
        began.elapsed() < Duration::from_millis(300),
        "the probe held the attempt up for {:?}",
        began.elapsed()
    );
    assert_eq!(
        one.nat().state,
        State::None,
        "a probe cut short left a result"
    );
    assert!(
        matches!(
            one.probe_nat(vec![silent], Duration::from_secs(1)),
            Err(Error::Busy)
        ),
        "a probe ran while the attempt held the port"
    );
    one.end_connection("a");
}

/// An attempt's own reflexive answers fill the result once two server
/// addresses have answered: no probe needed where the configuration names
/// two.
#[test]
fn an_attempts_own_answers_fill_the_result() {
    let mut one = client();
    let (a, x) = server(Ipv4Addr::LOCALHOST, None, 3, 1);
    let (b, y) = server(Ipv4Addr::new(127, 0, 0, 2), None, 3, 1);
    let config = Config {
        servers: vec![a.into(), b.into()],
        ..Config::default()
    };
    one.new_attempt("a", config, Transport::Bud)
        .expect("credentials");
    one.begin_p2p("a", &peer()).expect("the attempt began");
    until("the attempt's result", || {
        one.nat().source == Source::Attempt
    });
    x.join().unwrap();
    y.join().unwrap();
    let nat = one.nat();
    assert_eq!(nat.state, State::Done);
    assert_eq!(nat.mapping, Some(Mapping::Independent));
    assert_eq!((nat.asked, nat.answered), (2, 2));
    one.end_connection("a");
}
