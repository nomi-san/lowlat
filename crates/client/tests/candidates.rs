//! What an attempt offers: each address once, whichever probe found it.
//!
//! The repeat is read off a real attempt's host candidates, so the machine
//! needs an address to offer one.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use lowlat_client::config::{Backend, Decoding, Port};
use lowlat_client::{Client, Config, Event, Peer, Transport};
use lowlat_core::stun::{self, Message};

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

fn peer() -> Peer {
    let theirs = lowlat_crypto::credentials().expect("a peer's credentials");
    Peer {
        ufrag: theirs.ufrag,
        pwd: theirs.pwd,
        fingerprint: theirs.fingerprint,
        aes256: Some(theirs.aes256),
    }
}

/// Two reflexive servers on loopback, answered by one thread strictly in
/// order: the first reports `first` at the port it saw, and only then the
/// second reports `second`. Whatever offers the second answer has the first.
fn in_order(first: IpAddr, second: IpAddr) -> ([SocketAddr; 2], thread::JoinHandle<()>) {
    let sockets = [(); 2].map(|()| {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
    });
    let addrs = [0, 1].map(|index| sockets[index].local_addr().unwrap());
    let thread = thread::spawn(move || {
        for (socket, report) in sockets.iter().zip([first, second]) {
            let mut buf = [0u8; 256];
            loop {
                let Ok((len, from)) = socket.recv_from(&mut buf) else {
                    return;
                };
                let Ok(request) = Message::parse(&buf[..len]) else {
                    continue;
                };
                let seen = SocketAddr::new(report, from.port());
                let mut out = [0u8; 256];
                let len =
                    stun::encode_binding_response(&mut out, request.transaction_id(), seen, "any")
                        .unwrap();
                let _ = socket.send_to(&out[..len], from);
                break;
            }
        }
    });
    (addrs, thread)
}

/// **A reflexive answer that repeats a host candidate is not offered again.**
/// A server that reports one of this machine's own addresses at the attempt's
/// port -- what every server on IPv6 does, where nothing translates -- names a
/// candidate the attempt raised when it began; a server that reports anything
/// else is still offered.
#[test]
fn a_reflexive_answer_that_repeats_a_host_candidate_is_not_offered_again() {
    let host = *lowlat_net::host_addresses(false)
        .first()
        .expect("no host candidate: this machine offers no private or IPv6 address");
    let elsewhere = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
    let (servers, answering) = in_order(host, elsewhere);

    let mut one = client();
    let config = Config {
        servers: servers.to_vec(),
        ..Config::default()
    };
    one.new_attempt("a", config, Transport::Bud)
        .expect("credentials");
    one.begin_p2p("a", &peer()).expect("the attempt began");

    let mut offered: Vec<SocketAddr> = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while !offered.iter().any(|addr| addr.ip() == elsewhere) {
        assert!(
            Instant::now() < end,
            "the second server's answer was never offered: {offered:?}"
        );
        while let Some(received) = one.poll_event() {
            if let Event::Candidate { addr, .. } = received.event {
                offered.push(addr);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
    answering.join().unwrap();
    one.end_connection("a");

    assert_eq!(
        offered.iter().filter(|addr| addr.ip() == host).count(),
        1,
        "a host candidate was offered again: {offered:?}"
    );
}
