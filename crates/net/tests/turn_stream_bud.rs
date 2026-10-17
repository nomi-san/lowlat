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

// SendRing capacity is the fragment body, not the encrypted UDP wire ceiling.
const SLOT: usize = lowlat_core::DEFAULT_BODY;
const SLOTS: usize = 64;
const CHANNEL: u8 = lowlat_core::control::CONTROL_CHANNEL;
const KEY: [u8; 32] = [0x77; 32];
const LEFT: (&str, &str) = ("aaaa", "passwordforaaaa");
const RIGHT: (&str, &str) = ("bbbb", "passwordforbbbb");

#[cfg(target_os = "linux")]
struct BlockUdp(u16);
#[cfg(target_os = "linux")]
impl BlockUdp {
    fn new(port: u16) -> Self {
        assert!(Self::rule("-I", port).success());
        Self(port)
    }
    fn rule(operation: &str, port: u16) -> std::process::ExitStatus {
        std::process::Command::new("sudo")
            .args([
                "-n",
                "iptables",
                operation,
                "OUTPUT",
                "-p",
                "udp",
                "--sport",
                &port.to_string(),
                "-j",
                "DROP",
            ])
            .status()
            .unwrap()
    }
}
#[cfg(target_os = "linux")]
impl Drop for BlockUdp {
    fn drop(&mut self) {
        let _ = Self::rule("-D", self.0);
    }
}

struct Arena {
    recv: [Vec<u8>; 3],
    recv_meta: [Vec<SlotMeta>; 3],
    send: [Vec<u8>; 3],
    send_meta: [Vec<SendSlot>; 3],
}
impl Arena {
    fn new() -> Self {
        Self {
            recv: std::array::from_fn(|_| vec![0; SLOT * SLOTS]),
            recv_meta: std::array::from_fn(|_| vec![SlotMeta::default(); SLOTS]),
            send: std::array::from_fn(|_| vec![0; SLOT * SLOTS]),
            send_meta: std::array::from_fn(|_| vec![SendSlot::default(); SLOTS]),
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
        for (channel, (bodies, meta)) in self
            .recv
            .iter_mut()
            .zip(self.recv_meta.iter_mut())
            .enumerate()
        {
            session
                .attach_recv(
                    u8::try_from(channel).unwrap(),
                    RecvRing::new(bodies, meta, SLOT).unwrap(),
                )
                .unwrap();
        }
        for (channel, (bodies, meta)) in self
            .send
            .iter_mut()
            .zip(self.send_meta.iter_mut())
            .enumerate()
        {
            let channel = u8::try_from(channel).unwrap();
            session
                .attach_send(channel, SendRing::new(bodies, meta, SLOT, channel).unwrap())
                .unwrap();
        }
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
        #[cfg(target_os = "linux")]
        let _udp_block = if mode != 0 && std::env::var_os("LOWLAT_TEST_BLOCK_CLIENT_UDP").is_some()
        {
            Some(BlockUdp::new(client.socket().local_addr().unwrap().port()))
        } else {
            None
        };
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
        // Synthetic opaque video/audio bodies, not codec playback. Exercise
        // fragmentation, all native channel numbers and byte preservation.
        let video: Vec<u8> = (0..32_768)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        let audio = [0x53; 1024];
        host.endpoint()
            .session()
            .send_message(1, b"", &video)
            .unwrap();
        host.endpoint()
            .session()
            .send_message(2, b"", &audio)
            .unwrap();
        let mut media_output = vec![0; video.len()];
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut video_ok, mut audio_ok) = (false, false);
        while Instant::now() < deadline && (!video_ok || !audio_ok) {
            client.turn(|_| {}).unwrap();
            host.turn(|_| {}).unwrap();
            if let Some(Ok(len)) = client
                .endpoint()
                .session()
                .take_message(1, &mut media_output)
            {
                assert_eq!(&media_output[..len], video.as_slice());
                video_ok = true;
            }
            if let Some(Ok(len)) = client
                .endpoint()
                .session()
                .take_message(2, &mut media_output)
            {
                assert_eq!(&media_output[..len], audio.as_slice());
                audio_ok = true;
            }
        }
        assert!(
            video_ok && audio_ok,
            "carrier {mode} lost fragmented video/audio bodies"
        );
        assert_eq!(
            host.endpoint().path(),
            client.endpoint().relay().and_then(Relay::relayed)
        );
        let seconds: u64 = std::env::var("LOWLAT_TEST_TURN_SOAK_SECONDS")
            .unwrap_or_else(|_| "0".into())
            .parse()
            .unwrap();
        let started = Instant::now();
        let mut tick = Instant::now();
        while started.elapsed() < Duration::from_secs(seconds) {
            client.turn(|_| {}).unwrap();
            host.turn(|_| {}).unwrap();
            assert!(matches!(
                client.endpoint().relay().unwrap().state(),
                lowlat_core::relay::State::Ready(_)
            ));
            if Instant::now() >= tick {
                client
                    .endpoint()
                    .session()
                    .send_message(CHANNEL, b"", b"keepalive")
                    .unwrap();
                host.endpoint()
                    .session()
                    .send_message(CHANNEL, b"", b"keepalive")
                    .unwrap();
                tick = Instant::now() + Duration::from_secs(2);
            }
            while client
                .endpoint()
                .session()
                .take_message(CHANNEL, &mut output)
                .is_some()
            {}
            while host
                .endpoint()
                .session()
                .take_message(CHANNEL, &mut output)
                .is_some()
            {}
        }
        client
            .endpoint()
            .session()
            .send_message(CHANNEL, b"", b"after-renewal")
            .unwrap();
        host.endpoint()
            .session()
            .send_message(CHANNEL, b"", b"after-renewal")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut left_ok, mut right_ok) = (false, false);
        while Instant::now() < deadline && (!left_ok || !right_ok) {
            client.turn(|_| {}).unwrap();
            host.turn(|_| {}).unwrap();
            while let Some(Ok(len)) = client
                .endpoint()
                .session()
                .take_message(CHANNEL, &mut output)
            {
                left_ok |= &output[..len] == b"after-renewal";
            }
            while let Some(Ok(len)) = host.endpoint().session().take_message(CHANNEL, &mut output) {
                right_ok |= &output[..len] == b"after-renewal";
            }
        }
        assert!(
            left_ok && right_ok,
            "carrier {mode} lost media after renewal"
        );
        client.endpoint().relay_mut().unwrap().release();
        client.turn(|_| {}).unwrap();
        client.finish_output(Duration::from_millis(100)).unwrap();
        eprintln!(
            "fixture carrier {mode}: authenticated traffic survived {seconds}s and clean output drain"
        );
    }
}
