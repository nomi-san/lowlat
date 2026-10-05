//! The port every attempt binds: the stable one at every attempt, the next
//! free one up while it is taken, and the stable one again once it is free.
//!
//! The port is read off the attempt's host candidates, so the machine needs a
//! private address to offer one.

use std::net::UdpSocket;
use std::num::NonZeroU16;
use std::time::{Duration, Instant};

use lowlat_client::config::{Backend, Decoding, Port};
use lowlat_client::{Client, Config, Event, Peer, Transport};

/// A client with no decoder whose attempts bind `first`, and no mapping.
fn client(first: NonZeroU16) -> Client {
    Client::new(
        &Decoding {
            backend: Backend::None,
            ..Default::default()
        },
        Port::Stable {
            first,
            mapping: None,
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

/// Begin an attempt toward a peer that never answers, and the port its host
/// candidates carry.
fn attempt_port(client: &mut Client) -> u16 {
    client
        .new_attempt("a", Config::default(), Transport::Bud)
        .expect("credentials");
    let theirs = lowlat_crypto::credentials().expect("a peer's credentials");
    client
        .begin_p2p(
            "a",
            &Peer {
                ufrag: theirs.ufrag,
                pwd: theirs.pwd,
                fingerprint: theirs.fingerprint,
                aes256: Some(theirs.aes256),
            },
        )
        .expect("the attempt began");
    let began = Instant::now();
    let mut port = None;
    while port.is_none() && began.elapsed() < Duration::from_secs(5) {
        while let Some(received) = client.poll_event() {
            if let Event::Candidate {
                addr, lan: true, ..
            } = received.event
            {
                port = Some(addr.port());
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    client.end_connection("a");
    port.expect("no host candidate: this machine offers no private address")
}

#[test]
fn every_attempt_binds_the_stable_port_and_walks_past_a_taken_one() {
    let first = free_port();
    let mut client = client(first);
    assert_eq!(attempt_port(&mut client), first.get());
    assert_eq!(
        attempt_port(&mut client),
        first.get(),
        "the second attempt moved"
    );

    // Taken by something else: the next free one up, within the walk.
    let held = UdpSocket::bind(("0.0.0.0", first.get())).unwrap();
    // Where the system lets the families be bound apart, take both.
    let held_v6 = UdpSocket::bind(("::", first.get()));
    let walked = attempt_port(&mut client);
    assert!(
        walked > first.get() && walked < first.get() + 50,
        "a taken port walked to {walked} from {first}"
    );
    drop((held, held_v6));

    // Free again: the next attempt asks for the stable one first.
    assert_eq!(attempt_port(&mut client), first.get());
}
