//! Hermetic coturn interoperability: TURN carrier on one side, authenticated
//! BUD/UDP on the other. No Parsec accounts, desktop or GPU/decoder required.
#![cfg(feature = "turn-stream")]

use lowlat_core::{
    channel::{RecvRing, SlotMeta},
    conn::{Conn, Credentials, Kind},
    endpoint::Endpoint,
    envelope::Envelope,
    relay::{Relay, Transport},
    send::{SendRing, SendSlot},
    session::Session,
};
use lowlat_net::{
    Shell, Socket, Wake,
    stream_io::{Security, Tls},
};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const SLOT: usize = 256;
const SLOTS: usize = 64;
const CHANNEL: u8 = 1;
const KEY: [u8; 32] = [0x77; 32];
const LEFT: (&str, &str) = ("aaaa", "passwordforaaaa");
const RIGHT: (&str, &str) = ("bbbb", "passwordforbbbb");

struct Arena {
    recv: Vec<u8>,
    recv_meta: Vec<SlotMeta>,
    send: Vec<u8>,
    send_meta: Vec<SendSlot>,
}
impl Arena {
    fn new() -> Self {
        Self {
            recv: vec![0; SLOT * SLOTS],
            recv_meta: vec![SlotMeta::default(); SLOTS],
            send: vec![0; SLOT * SLOTS],
            send_meta: vec![SendSlot::default(); SLOTS],
        }
    }
    fn endpoint(
        &mut self,
        ours: (&'static str, &'static str),
        theirs: (&'static str, &'static str),
        seed: u8,
        relay: Option<Relay<'static>>,
    ) -> Endpoint<'_> {
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
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        session
            .attach_recv(
                CHANNEL,
                RecvRing::new(&mut self.recv, &mut self.recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CHANNEL,
                SendRing::new(&mut self.send, &mut self.send_meta, SLOT, CHANNEL).unwrap(),
            )
            .unwrap();
        match relay {
            Some(relay) => Endpoint::relayed(conn, session, relay),
            None => Endpoint::new(conn, session),
        }
    }
}

#[test]
#[ignore = "requires scripts/test-turn-stream.sh isolated coturn fixture"]
fn turn_stream_bud_coturn_udp_tcp_tls_bidirectional() {
    let server: SocketAddr = std::env::var("LOWLAT_TEST_TURN_ADDR")
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        !server.ip().is_loopback(),
        "fixture relay/peer address must be non-loopback"
    );
    let ca = std::fs::read(std::env::var("LOWLAT_TEST_TURN_CA").unwrap()).unwrap();
    for mode in 0..3 {
        let mut address = server;
        if mode == 2 {
            address.set_port(
                std::env::var("LOWLAT_TEST_TURN_TLS_PORT")
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
        }
        let mut wake = match mode {
            0 => Wake::new().unwrap(),
            1 => Wake::stream(address, Security::Tcp).unwrap(),
            _ => {
                let mut roots = rustls::RootCertStore::empty();
                roots
                    .add(rustls::pki_types::CertificateDer::from(ca.clone()))
                    .unwrap();
                Wake::stream(
                    address,
                    Security::Tls(Tls::with_roots("relay.test", roots).unwrap()),
                )
                .unwrap()
            }
        };
        wake.prepare(|| false).unwrap();
        let mut client_arena = Arena::new();
        let mut host_arena = Arena::new();
        let client_endpoint = client_arena.endpoint(
            LEFT,
            RIGHT,
            0xA1,
            Some(Relay::new_on(
                address,
                "fixture",
                "fixture-password",
                [0x3C; 16],
                0.0,
                if mode == 0 {
                    Transport::Datagram
                } else {
                    Transport::Reliable
                },
            )),
        );
        let mut client = Shell::new(Socket::open(0).unwrap(), wake, client_endpoint);
        let mut host = Shell::new(
            Socket::open(0).unwrap(),
            Wake::new().unwrap(),
            host_arena.endpoint(RIGHT, LEFT, 0xB2, None),
        );
        let host_address = SocketAddr::new(server.ip(), host.socket().local_addr().unwrap().port());
        client
            .endpoint()
            .add_candidate(host_address, Kind::Reflexive)
            .unwrap();
        client.endpoint().conn().set_peer_ready();
        client
            .endpoint()
            .session()
            .send_message(CHANNEL, b"left:", b"authenticated BUD")
            .unwrap();
        host.endpoint()
            .session()
            .send_message(CHANNEL, b"right:", b"authenticated BUD")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut offered = false;
        let mut left = None;
        let mut right = None;
        let mut output = [0; 512];
        while Instant::now() < deadline && (left.is_none() || right.is_none()) {
            client.turn(|_| {}).unwrap();
            if !offered && let Some(relayed) = client.endpoint().relay().and_then(Relay::relayed) {
                host.endpoint()
                    .add_candidate(relayed, Kind::Reflexive)
                    .unwrap();
                host.endpoint().conn().set_peer_ready();
                offered = true;
            }
            host.turn(|_| {}).unwrap();
            if left.is_none()
                && let Some(Ok(len)) = client
                    .endpoint()
                    .session()
                    .take_message(CHANNEL, &mut output)
            {
                left = Some(output[..len].to_vec());
            }
            if right.is_none()
                && let Some(Ok(len)) = host.endpoint().session().take_message(CHANNEL, &mut output)
            {
                right = Some(output[..len].to_vec());
            }
        }
        eprintln!(
            "fixture carrier {mode}: relay={:?} client_path={:?} host_path={:?} client_io={:?} host_io={:?}",
            client.endpoint().relay().unwrap().state(),
            client.endpoint().path(),
            host.endpoint().path(),
            client.stats(),
            host.stats()
        );
        assert_eq!(
            left.as_deref(),
            Some(b"right:authenticated BUD".as_slice()),
            "carrier {mode}"
        );
        assert_eq!(
            right.as_deref(),
            Some(b"left:authenticated BUD".as_slice()),
            "carrier {mode}"
        );
        assert_eq!(client.endpoint().path(), Some(host_address));
        assert!(offered);
    }
}
