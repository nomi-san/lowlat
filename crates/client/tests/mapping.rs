//! A handle that keeps its port mapped holds the port from creation: a second
//! handle with the same seed walks past it then, before anything is mapped,
//! and never maps, moves or deletes the first handle's entry.

use std::net::UdpSocket;
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use lowlat_client::config::{Backend, Decoding, Mapping, Port};
use lowlat_client::{Client, Config, Peer, Transport};
use lowlat_portmap::fake::{Behaviour, Fake};

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
