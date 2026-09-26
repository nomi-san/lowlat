//! Phase 13: two shells, one in each role, over real sockets.
//!
//! The simulator pair proves the state machines; this proves the loop
//! around them -- the shell's own timers, the platform's wake, a real socket --
//! carries a browser session from punch to picture the way it carries a
//! native one. Same loop, second instantiation.

// The loop is built where a platform's system calls are written.
#![cfg(any(target_os = "linux", windows))]
// Fixtures build bytes from loop counters and percentiles from lengths; the
// truncating casts are the obvious ones.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use lowlat_core::conn::{Conn, Credentials, Kind};
use lowlat_core::endpoint::{Endpoint, Media};
use lowlat_crypto::cert::{Certificate, FINGERPRINT_LEN};
use lowlat_net::web::{Role, WebSession};
use lowlat_net::{Shell, Socket, Wake};

const LEFT: (&str, &str) = ("aaaaaaaa", "passwordforaaaaaaaaaaaaa");
const RIGHT: (&str, &str) = ("bbbbbbbb", "passwordforbbbbbbbbbbbbb");

fn shell(
    ours: (&'static str, &'static str),
    theirs: (&'static str, &'static str),
    seed: u8,
    role: Role,
    expect: [u8; FINGERPRINT_LEN],
    identity: &Certificate,
) -> Shell<'static, WebSession> {
    let socket = Socket::open(0).expect("socket");
    let wake = Wake::new().expect("wake");
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
    let session = WebSession::new(role, Some(expect), identity, 1, 0.0).expect("session");
    Shell::new(socket, wake, Endpoint::new(conn, session))
}

fn loopback_of(shell: &Shell<'_, WebSession>) -> SocketAddr {
    let mut addr = shell.socket().local_addr().expect("addr");
    addr.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
    addr
}

/// Two shells punch over loopback, handshake, associate, and carry a frame
/// each way. The whole loop, end to end, on the second pipe.
#[test]
fn two_shells_punch_then_secure_then_carry_a_frame() {
    let ours = Certificate::generate().unwrap();
    let theirs = Certificate::generate().unwrap();
    let mut left = shell(
        LEFT,
        RIGHT,
        0xA1,
        Role::Client,
        *theirs.fingerprint(),
        &ours,
    );
    let mut right = shell(
        RIGHT,
        LEFT,
        0xB2,
        Role::Server,
        *ours.fingerprint(),
        &theirs,
    );

    let left_addr = loopback_of(&left);
    let right_addr = loopback_of(&right);
    left.endpoint()
        .conn()
        .add_candidate(right_addr, Kind::Reflexive)
        .unwrap();
    right
        .endpoint()
        .conn()
        .add_candidate(left_addr, Kind::Reflexive)
        .unwrap();
    left.endpoint().conn().set_peer_ready();
    right.endpoint().conn().set_peer_ready();

    // Queued before any path exists: it must wait, not vanish.
    let frame: Vec<u8> = (0..200 * 1024).map(|i| (i % 241) as u8).collect();
    left.endpoint()
        .session()
        .send_message(1, b"VIDEO-HDR!", &frame)
        .unwrap();
    left.endpoint()
        .session()
        .send_message(0, b"HDR-13-BYTES!", b"hello")
        .unwrap();

    let started = Instant::now();
    let mut out = vec![0u8; 1 << 20];
    let mut got_frame = None;
    let mut got_control = None;
    let mut got_reply = None;
    let mut replied = false;
    let mut path_at = None;
    let mut up_at = None;
    while started.elapsed() < Duration::from_secs(6)
        && (got_frame.is_none() || got_control.is_none() || got_reply.is_none())
    {
        left.turn(|_| {}).expect("left turn");
        right.turn(|_| {}).expect("right turn");
        if path_at.is_none() && left.endpoint().path().is_some() {
            path_at = Some(started.elapsed());
        }
        if up_at.is_none() && left.endpoint().session().is_up() {
            up_at = Some(started.elapsed());
        }
        if let Some(fault) = left.endpoint().fault() {
            panic!("left faulted: {fault:?}");
        }
        if let Some(fault) = right.endpoint().fault() {
            panic!("right faulted: {fault:?}");
        }
        if got_frame.is_none()
            && let Some(Ok(len)) = right.endpoint().session().take_message(1, &mut out)
        {
            got_frame = Some(out[..len].to_vec());
        }
        if got_control.is_none()
            && let Some(Ok(len)) = right.endpoint().session().take_message(0, &mut out)
        {
            got_control = Some(out[..len].to_vec());
        }
        if got_control.is_some() && !replied {
            right
                .endpoint()
                .session()
                .send_message(2, b"AUDIO-HEADER-15", b"opus")
                .unwrap();
            replied = true;
        }
        if replied
            && got_reply.is_none()
            && let Some(Ok(len)) = left.endpoint().session().take_message(2, &mut out)
        {
            got_reply = Some(out[..len].to_vec());
        }
    }

    assert_eq!(
        left.endpoint().path(),
        Some(right_addr),
        "left found no path"
    );
    assert_eq!(
        right.endpoint().path(),
        Some(left_addr),
        "right found no path"
    );
    assert!(left.endpoint().session().is_up() && right.endpoint().session().is_up());
    assert_eq!(
        got_frame.as_deref(),
        Some(&frame[..]),
        "the frame did not cross whole"
    );
    assert_eq!(got_control.as_deref(), Some(&b"HDR-13-BYTES!hello"[..]));
    assert_eq!(got_reply.as_deref(), Some(&b"opus"[..]));
    println!(
        "web_shells: path at {:.0} ms, secure at {:.0} ms, a 200 KiB frame across at {:.0} ms",
        path_at.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
        up_at.map_or(0.0, |d| d.as_secs_f64() * 1000.0),
        started.elapsed().as_secs_f64() * 1000.0
    );
}

/// A clean close from one side is read as the session ending on the other,
/// with no fault: the alert crossed, not a silence.
#[test]
fn a_close_crosses_the_loopback_as_a_clean_end() {
    let ours = Certificate::generate().unwrap();
    let theirs = Certificate::generate().unwrap();
    let mut left = shell(
        LEFT,
        RIGHT,
        0xA3,
        Role::Client,
        *theirs.fingerprint(),
        &ours,
    );
    let mut right = shell(
        RIGHT,
        LEFT,
        0xB4,
        Role::Server,
        *ours.fingerprint(),
        &theirs,
    );
    let left_addr = loopback_of(&left);
    let right_addr = loopback_of(&right);
    left.endpoint()
        .conn()
        .add_candidate(right_addr, Kind::Reflexive)
        .unwrap();
    right
        .endpoint()
        .conn()
        .add_candidate(left_addr, Kind::Reflexive)
        .unwrap();
    left.endpoint().conn().set_peer_ready();
    right.endpoint().conn().set_peer_ready();

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(6)
        && !(left.endpoint().session().is_up() && right.endpoint().session().is_up())
    {
        left.turn(|_| {}).expect("left turn");
        right.turn(|_| {}).expect("right turn");
    }
    assert!(left.endpoint().session().is_up() && right.endpoint().session().is_up());

    left.endpoint().session().close();
    let mut ended = false;
    while started.elapsed() < Duration::from_secs(6) && !ended {
        left.turn(|_| {}).expect("left turn");
        let turn = right.turn(|_| {}).expect("right turn");
        ended = right.endpoint().health(turn.now) == lowlat_core::session::Health::Dead;
    }
    assert!(ended, "the close never reached the far side");
    assert_eq!(right.endpoint().fault(), None);
}
