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

// The seam under test and the host's admission are built on Linux so far
// (docs/impl-plan-windows.md).
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use lowlat_client::{Client, Config, Event, Peer, Transport};
use lowlat_core::conn::Kind;
use lowlat_host::admission::{self, Admission, Event as HostEvent};

fn host_config() -> admission::Config {
    admission::Config {
        microphone: None,
        pad_sink: None,
        exclusive_pointer: false,
        rumble_probe: false,
        exclusive_hold_ms: lowlat_host::floor::HOLD_MS,
        cg_level: 1,
        base_port: 0,
        shared_address_space: false,
        max_guests: 1,
        servers: Vec::new(),
        stream: None,
    }
}

fn host() -> Admission {
    Admission::new(host_config())
}

/// Both seams, the exchange relayed by hand, until the host's application
/// message comes back through the client. Returns whether it did.
fn session_under(legacy: bool) {
    let mut host = host();
    let mut client = Client::new(
        &lowlat_client::config::Decoding {
            backend: lowlat_client::config::Backend::None,
            ..Default::default()
        },
        lowlat_client::config::Port::Any,
    )
    .expect("a client without a decoder");
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

    // Input crosses the ring to the session thread and goes out: two key
    // messages, none dropped. The viewport is a request like any other.
    let before = client
        .telemetry()
        .control_out
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(client.set_viewport(lowlat_client::input::Viewport {
        x: 0,
        y: 0,
        w: 100,
        h: 100,
    }));
    for pressed in [true, false] {
        client
            .send_input(lowlat_client::input::Input::Key {
                code: 4,
                mods: 0,
                pressed,
            })
            .unwrap();
    }
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(2)
        && client
            .telemetry()
            .control_out
            .load(std::sync::atomic::Ordering::Relaxed)
            < before + 2
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        client
            .telemetry()
            .control_out
            .load(std::sync::atomic::Ordering::Relaxed),
        before + 2,
        "the key messages did not go out"
    );
    assert_eq!(
        client
            .telemetry()
            .input_dropped
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );

    // The departure is given its grace and no more: the session's loop
    // returns once the message has had it, and the leave ends there rather
    // than at the cap on waiting for it.
    let began = Instant::now();
    client.end_connection("a");
    let left = began.elapsed();
    assert!(
        left >= Duration::from_millis(200) && left < Duration::from_millis(450),
        "the departure took {left:?}"
    );
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

/// **A guest counts its stale fragments at the level its host was
/// configured with.** The stream's controller judges that count by its
/// level's ratio, and the count is the guest session's: a session built at
/// the default gave every level the default's thresholds under its own
/// ratio. Level 0 makes the difference plain -- its thresholds are zero, so
/// once a round trip is measured every fragment outstanding is stale -- where
/// the relaxed level, on loopback, finds almost none.
#[test]
#[ignore = "needs a hardware encoder"]
fn a_guest_counts_staleness_at_its_hosts_level() {
    let (stale, window, samples) = staleness_at(0);
    assert!(
        samples >= 20,
        "level 0: only {samples} samples had anything outstanding"
    );
    assert!(
        stale * 10 >= window * 9,
        "level 0 counted {stale} of {window} outstanding fragments stale"
    );
    let (stale, window, samples) = staleness_at(2);
    assert!(
        samples >= 20,
        "level 2: only {samples} samples had anything outstanding"
    );
    assert!(
        stale * 10 <= window,
        "level 2 counted {stale} of {window} outstanding fragments stale"
    );
}

/// The generator, coded by the vendor's encoder, at `level`.
fn generated(level: usize) -> lowlat_host::stream::Config {
    lowlat_host::stream::Config {
        audio: None,
        accept_microphone: false,
        audio_on: false,
        audio_kbps: 128,
        allow_raw_audio: false,
        codec: lowlat_host::stream::Codec::H264,
        ten_bit: false,
        chroma_444: false,
        backend: Some(lowlat_host::stream::Backend::Vendor),
        prefer_vulkan: false,
        width: 640,
        height: 360,
        fps: 60,
        configured_mbps: 5.0,
        min_mbps: 1.0,
        // Detail over the whole picture, so each one is several fragments
        // and a window is often open.
        detail_rows: 360,
        output: None,
        convert: None,
        display: false,
        full_fps: true,
        quality: lowlat_host::stream::Quality::default(),
        cg_level: level,
        virtual_fallback: false,
        wake_display: false,
        hold_memory_clock: false,
        own_display: lowlat_host::stream::OwnDisplay::default(),
    }
}

/// Stream to a client on loopback from a host at `level`, and add up what the
/// guest publishes whenever something is outstanding: the stale fragments,
/// the outstanding ones, and how many samples saw any.
fn staleness_at(level: usize) -> (u64, u64, usize) {
    let mut host = Admission::new(admission::Config {
        cg_level: level,
        stream: Some(generated(level)),
        ..host_config()
    });
    let mut client = Client::new(
        &lowlat_client::config::Decoding {
            backend: lowlat_client::config::Backend::None,
            ..Default::default()
        },
        lowlat_client::config::Port::Any,
    )
    .expect("a client without a decoder");
    let ours = client
        .new_attempt("a", Config::default(), Transport::Bud)
        .expect("credentials");
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

    // Nothing decodes here, so the units are let go as they arrive: a full
    // ring would hold the acknowledgements back and age every fragment.
    let units = client.units();
    let (mut stale, mut window, mut samples) = (0u64, 0u64, 0usize);
    let mut up_at: Option<Instant> = None;
    let began = Instant::now();
    // **Until enough samples saw a window open**: on loopback a fragment is
    // acknowledged within a fraction of a millisecond, so most samples see
    // nothing outstanding at all.
    while began.elapsed() < Duration::from_secs(40)
        && samples < 200
        && up_at.is_none_or(|at| at.elapsed() < Duration::from_secs(10))
    {
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
                HostEvent::Ended { outcome, .. } => panic!("the host ended: {outcome:?}"),
                _ => {}
            }
        }
        while units.take().is_some() {}
        if let Some(guest) = host.guests().first() {
            let metrics = guest.metrics;
            if metrics.window > 0 && metrics.network_ms > 0.0 {
                up_at.get_or_insert_with(Instant::now);
                stale += u64::from(metrics.stale);
                window += u64::from(metrics.window);
                samples += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    client.end_connection("a");
    host.end_connection("a");
    println!("level {level}: {stale} of {window} outstanding stale over {samples} samples");
    (stale, window, samples)
}

/// A pad is one family until it is unplugged, and the rule is applied where
/// the application calls, not on the session thread: a state for a pad sent
/// as its own reports is refused at once, and so is a report for a pad sent
/// as states; a report this path does not carry is refused as such; an
/// unplug forgets the pad. No session is needed for any of it -- the family
/// is recorded with the attempt -- and without one the sending fails as
/// "no session" rather than silently.
#[test]
fn a_pad_is_one_family_until_it_is_unplugged() {
    use lowlat_client::Error;
    use lowlat_client::input::{Input, PadState, ReportKind};
    use lowlat_core::pad::Product;

    let mut client = Client::new(
        &lowlat_client::config::Decoding {
            backend: lowlat_client::config::Backend::None,
            ..Default::default()
        },
        lowlat_client::config::Port::Any,
    )
    .expect("a client without a decoder");
    let idle = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
    let state = Input::PadState {
        pad: 1,
        state: PadState::default(),
    };

    // Nothing is recorded without an attempt.
    assert_eq!(
        client.send_pad_report(1, Product::DualSense, ReportKind::Input, idle),
        Err(Error::NoSession)
    );
    client
        .new_attempt("a", Config::default(), Transport::Bud)
        .expect("credentials");

    // A report pad: the family is recorded even though no session carries
    // the report yet, and a state for it is refused from then on.
    assert_eq!(
        client.send_pad_report(1, Product::DualSense, ReportKind::Input, idle),
        Err(Error::NoSession)
    );
    assert_eq!(client.send_input(state), Err(Error::PadFamily));
    assert_eq!(
        client.send_input(Input::PadButton {
            pad: 1,
            button: 0,
            pressed: true
        }),
        Err(Error::PadFamily)
    );
    // A state pad: a report for it is refused.
    assert_eq!(
        client.send_input(Input::PadState {
            pad: 2,
            state: PadState::default()
        }),
        Err(Error::NoSession)
    );
    assert_eq!(
        client.send_pad_report(2, Product::DualShock4, ReportKind::Input, idle),
        Err(Error::PadFamily)
    );
    // A report this path does not carry is refused before any family is
    // recorded: the wrong length, a feature report other than the two.
    assert_eq!(
        client.send_pad_report(3, Product::DualSense, ReportKind::Input, &idle[..40]),
        Err(Error::Report)
    );
    assert_eq!(
        client.send_pad_report(3, Product::DualSense, ReportKind::Feature, &[0x09; 20]),
        Err(Error::Report)
    );
    assert_eq!(
        client.send_input(Input::PadState {
            pad: 3,
            state: PadState::default()
        }),
        Err(Error::NoSession)
    );
    // An unplug forgets the family, and the other one may follow.
    assert_eq!(
        client.send_input(Input::PadUnplug { pad: 1 }),
        Err(Error::NoSession)
    );
    assert_eq!(client.send_input(state), Err(Error::NoSession));
    assert_eq!(
        client.send_pad_report(1, Product::DualSense, ReportKind::Input, idle),
        Err(Error::PadFamily)
    );
}
