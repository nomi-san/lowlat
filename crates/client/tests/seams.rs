//! The client's seam against the host's, over real sockets on loopback,
//! under each cipher.
//!
//! The hermetic test drives the driver under the simulator; this drives the
//! thread and the socket the seam really spawns, against the host crate's
//! own admission, which is the peer every other test here stands in for.
//! What it proves is the keying: an exchange with a media key seals the
//! session under the 256-bit cipher and one without under the legacy
//! 128-bit one, and in both the initialization crosses and a message from
//! the host's application comes back down the same session.

use std::time::{Duration, Instant};

use lowlat_client::{Client, Config, Event, Peer, Transport};
use lowlat_core::conn::Kind;
use lowlat_host::admission::{self, Admission, Event as HostEvent};

fn host() -> Admission {
    Admission::new(admission::Config {
        microphone: None,
        exclusive_pointer: false,
        rumble_probe: false,
        exclusive_hold_ms: lowlat_host::floor::HOLD_MS,
        cg_level: 1,
        base_port: 0,
        shared_address_space: false,
        max_guests: 1,
        servers: Vec::new(),
        stream: None,
    })
}

/// Both seams, the exchange relayed by hand, until the host's application
/// message comes back through the client. Returns whether it did.
fn session_under(legacy: bool) {
    let mut host = host();
    let mut client = Client::new();
    let config = Config {
        legacy_cipher: legacy,
        ..Config::default()
    };

    let ours = client
        .new_attempt("a", config, Transport::Bud)
        .expect("credentials");
    assert_eq!(
        ours.aes256.is_empty(),
        legacy,
        "the offer's media key did not follow the setting"
    );
    host.new_attempt(
        "a",
        admission::Peer {
            ufrag: ours.ufrag.clone(),
            pwd: ours.pwd.clone(),
            aes256: (!ours.aes256.is_empty()).then(|| ours.aes256.clone()),
            transport: admission::Transport::Bud,
            fingerprint: Some(ours.fingerprint.clone()),
            permissions: lowlat_host::inject::Permissions::default(),
            owner: false,
        },
    )
    .expect("the host registered the offer");
    let theirs = host.begin_p2p("a", 0).expect("the host answered");
    assert_eq!(
        theirs.aes256.is_empty(),
        legacy,
        "the answer's media key did not follow the offer"
    );
    client
        .begin_p2p(
            "a",
            &Peer {
                ufrag: theirs.ufrag.clone(),
                pwd: theirs.pwd.clone(),
                fingerprint: theirs.fingerprint.clone(),
                aes256: (!theirs.aes256.is_empty()).then(|| theirs.aes256.clone()),
            },
        )
        .expect("the client began");

    let began = Instant::now();
    let mut client_up = false;
    let mut host_up = false;
    let mut said = false;
    let mut heard: Option<Vec<u8>> = None;
    while began.elapsed() < Duration::from_secs(20) && heard.is_none() {
        // Relay every candidate each way, as a signaling service would.
        while let Some(received) = client.poll_event() {
            match received.event {
                Event::Candidate {
                    addr,
                    from_stun,
                    lan,
                } => {
                    host.add_candidate("a", addr, false, Kind::marked(lan, from_stun));
                }
                Event::Ready => {
                    host.add_candidate("a", "1.2.3.4:1234".parse().unwrap(), true, Kind::Direct);
                }
                Event::Established { .. } => client_up = true,
                Event::UserData { id, text } => {
                    assert_eq!(id, 5);
                    heard = Some(text);
                }
                Event::Ended { outcome } => panic!("the client ended: {outcome:?}"),
                _ => {}
            }
        }
        while let Some(received) = host.poll_event() {
            match received.event {
                HostEvent::Candidate {
                    addr,
                    from_stun,
                    lan,
                    ..
                } => client.add_candidate("a", addr, false, Kind::marked(lan, from_stun)),
                HostEvent::Ready { .. } => {
                    client.add_candidate("a", "1.2.3.4:1234".parse().unwrap(), true, Kind::Direct);
                }
                HostEvent::Established { .. } => host_up = true,
                HostEvent::Ended { outcome, .. } => panic!("the host ended: {outcome:?}"),
                _ => {}
            }
        }
        // Once both are up, give the initialization a moment to cross, then
        // say something down the session.
        if client_up && host_up && !said {
            std::thread::sleep(Duration::from_millis(200));
            let guests = host.guests();
            assert_eq!(guests.len(), 1, "the host has no guest");
            assert!(host.send_user_data(guests[0].number, 5, b"hello"));
            said = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(client_up && host_up, "the pair did not establish");
    assert_eq!(
        heard.as_deref(),
        Some(&b"hello"[..]),
        "the host's message did not arrive"
    );
    assert_eq!(
        client
            .telemetry()
            .state
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    client.end_connection("a");
    // The host reads the departure and reports it.
    let began = Instant::now();
    let mut left = false;
    while began.elapsed() < Duration::from_secs(5) && !left {
        while let Some(received) = host.poll_event() {
            if let HostEvent::Ended { outcome, .. } = received.event {
                assert_eq!(outcome, admission::Outcome::PeerLeft(0));
                left = true;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(left, "the host never saw the client leave");
    host.end_connection("a");
}

#[test]
fn a_session_under_the_current_cipher() {
    session_under(false);
}

#[test]
fn a_session_under_the_legacy_cipher() {
    session_under(true);
}
