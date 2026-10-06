//! A handle that keeps its port mapped holds the port from creation: a second
//! handle with the same seed walks past it then, before anything is mapped,
//! and never maps, moves or deletes the first handle's entry.

mod support;

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use lowlat_client::config::{Backend, Decoding, Mapping, Port};
use lowlat_client::nat::State;
use lowlat_client::{Client, Config, Peer, Transport};
use lowlat_portmap::Protocol;
use lowlat_portmap::fake::{Behaviour, EXTERNAL, Fake};

/// A client with no decoder whose port is `first` and kept mapped on `fake`.
fn client(first: NonZeroU16, fake: &Fake) -> Client {
    Client::new(
        &Decoding {
            backend: Backend::None,
            ..Default::default()
        },
        Port::Stable {
            first,
            mapping: Some(Mapping {
                description: "ll-test".into(),
                gateway: Some(fake.gateway()),
            }),
        },
    )
    .expect("a client without a decoder")
}

/// A port free on this machine now, low enough that the walk above it has
/// room before the top of the range.
fn free_port() -> NonZeroU16 {
    loop {
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        if port <= 65_000 {
            return NonZeroU16::new(port).unwrap();
        }
    }
}

/// Wait for `holds`, failing with `what` after a generous bound.
fn until(what: &str, mut holds: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !holds() {
        assert!(Instant::now() < end, "never: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_second_handle_with_one_seed_never_touches_the_first_ones_mapping() {
    let fake = Fake::start(Behaviour::default());
    let first = free_port();
    let one = client(first, &fake);
    until("the first handle's mapping", || {
        fake.table().contains_key(&first.get())
    });
    // Held between attempts: nothing else binds it.
    assert!(
        UdpSocket::bind(("0.0.0.0", first.get())).is_err(),
        "the first handle's port was not held"
    );

    let two = client(first, &fake);
    until("the second handle's mapping", || fake.table().len() == 2);
    let theirs: Vec<u16> = fake
        .table()
        .keys()
        .copied()
        .filter(|&port| port != first.get())
        .collect();
    assert_eq!(theirs.len(), 1, "{:?}", fake.table());
    assert!(theirs[0] > first.get(), "the second handle did not walk up");

    drop(two);
    let table = fake.table();
    assert!(
        table.contains_key(&first.get()),
        "the second handle deleted the first's mapping"
    );
    assert!(
        !table.contains_key(&theirs[0]),
        "the second handle left its own"
    );
    assert_eq!(fake.calls("pcp-delete", first.get()), 0);

    drop(one);
    assert!(fake.table().is_empty(), "left behind: {:?}", fake.table());
}

/// An attempt has the mapping looked at again as it begins: one a gateway
/// lost by restarting is made again at once, not at the renewal. The attempt
/// is lent the held port, and the port is held again once it has ended.
#[test]
fn an_attempt_looks_again_and_the_port_is_held_again_after_it() {
    let fake = Fake::start(Behaviour::default());
    let first = free_port();
    let mut one = Client::new(
        &Decoding {
            backend: Backend::None,
            ..Default::default()
        },
        Port::Stable {
            first,
            mapping: Some(Mapping {
                description: "ll-test".into(),
                // Renewed after half a minute, unless a look comes first.
                gateway: Some(fake.gateway_with_lifetime(60)),
            }),
        },
    )
    .expect("a client without a decoder");
    until("the mapping", || fake.table().contains_key(&first.get()));
    // Past the floor between two looks.
    std::thread::sleep(Duration::from_millis(200));
    fake.restart();

    one.new_attempt("a", Config::default(), Transport::Bud)
        .expect("credentials");
    let theirs = lowlat_crypto::credentials().expect("a peer's credentials");
    one.begin_p2p(
        "a",
        &Peer {
            ufrag: theirs.ufrag,
            pwd: theirs.pwd,
            fingerprint: theirs.fingerprint,
            aes256: Some(theirs.aes256),
        },
    )
    .expect("the attempt began");
    let end = Instant::now() + Duration::from_secs(1);
    while !fake.table().contains_key(&first.get()) {
        assert!(Instant::now() < end, "not made again as the attempt began");
        std::thread::sleep(Duration::from_millis(10));
    }
    one.end_connection("a");
    assert!(
        UdpSocket::bind(("0.0.0.0", first.get())).is_err(),
        "the port was not held again after the attempt"
    );
    drop(one);
    assert!(fake.table().is_empty(), "left behind: {:?}", fake.table());
}

/// A probe asks from the port the handle holds, and gives it back. Against
/// the gateway's mapping: its own address seen at a port per destination is
/// a mapping reached all the same, numbered 2, the raw mapping beside it.
#[test]
fn a_probe_asks_from_the_held_port_and_a_confirmed_mapping_numbers_two() {
    let fake = Fake::start(Behaviour::default());
    let first = free_port();
    let mut one = client(first, &fake);
    until("the mapping", || fake.table().contains_key(&first.get()));

    let (a, x) = support::server(Ipv4Addr::LOCALHOST, Some(EXTERNAL), 1, 1);
    let (b, y) = support::server(Ipv4Addr::new(127, 0, 0, 2), Some(EXTERNAL), 2, 1);
    one.probe_nat(vec![a, b], Duration::from_secs(3))
        .expect("a probe");
    until("the result", || one.nat().state == State::Done);
    x.join().unwrap();
    y.join().unwrap();
    let nat = one.nat();
    assert_eq!(
        nat.public,
        Some(SocketAddr::from((EXTERNAL, first.get() + 1))),
        "asked from another port than the one held"
    );
    assert_eq!(nat.mapping, Some(lowlat_core::nat::Mapping::Dependent));
    assert_eq!(
        (nat.gateway, nat.confirmed, nat.double),
        (Protocol::Pcp, true, false)
    );
    assert_eq!(nat.number, Some(2));
    assert!(
        UdpSocket::bind(("0.0.0.0", first.get())).is_err(),
        "the port was not given back"
    );
    drop(one);
    assert!(fake.table().is_empty(), "left behind: {:?}", fake.table());
}

/// The translation in front of this machine's port on the network it is on,
/// asked of the servers `LOWLAT_STUN` names: plain, then with the port mapped
/// on the gateway, as a client keeps it. Run by hand, beside another tool's
/// answer.
#[test]
#[ignore = "live: asks this network's gateway and public reflexive servers"]
fn live_translation_on_this_network() {
    let servers: Vec<SocketAddrV4> = std::env::var("LOWLAT_STUN")
        .unwrap_or_default()
        .split(',')
        .filter_map(|name| {
            lowlat_net::addrs::resolve_server(name.trim())
                .into_iter()
                .find_map(|addr| match addr {
                    SocketAddr::V4(v4) => Some(v4),
                    SocketAddr::V6(_) => None,
                })
        })
        .collect();
    assert!(servers.len() >= 2, "fewer than two servers: {servers:?}");
    let probe = |client: &mut Client, what: &str| {
        client
            .probe_nat(servers.clone(), Duration::from_secs(5))
            .expect("a probe");
        until("the probe's result", || client.nat().state == State::Done);
        let nat = client.nat();
        println!(
            "live {what}: type={:?} mapping={:?} public={:?} kept={} answered={}/{} \
             gateway={:?} confirmed={} double={} carrier={}",
            nat.number,
            nat.mapping,
            nat.public,
            nat.port_preserved,
            nat.answered,
            nat.asked,
            nat.gateway,
            nat.confirmed,
            nat.double,
            nat.carrier
        );
    };
    let decoding = Decoding {
        backend: Backend::None,
        ..Default::default()
    };
    let mut plain = Client::new(&decoding, Port::Any).expect("a client");
    probe(&mut plain, "plain");
    drop(plain);

    let mut mapped = Client::new(
        &decoding,
        Port::Stable {
            first: free_port(),
            mapping: Some(Mapping {
                description: "ll-live".into(),
                gateway: None,
            }),
        },
    )
    .expect("a client");
    let end = Instant::now() + Duration::from_secs(15);
    while mapped.mapping().is_none_or(|status| status.port == 0) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("live mapping: {:?}", mapped.mapping());
    probe(&mut mapped, "mapped");
}
