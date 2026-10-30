//! A relay attempt over TCP through the real loop (docs/03-connectivity.md
//! 7.4 and 9): the stream's connect, its end and the clean leave, each
//! ending the attempt as the relay's state says.
//!
//! **The relay is the standard's, behind a real listener.** The simulator's
//! relay -- written from the standard, sharing nothing with the core -- is
//! handed what the client writes on a loopback connection and writes its
//! answers back, so the client's loop, its stream and its codec meet a relay
//! that is not a third reading of the standard.

#![cfg(any(target_os = "linux", target_os = "macos", windows))]

use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use lowlat_client::config::{Backend, Carrier, Decoding, Port, Relay as RelayConfig};
use lowlat_client::{Client, Config, Event, Outcome, Peer, Transport};
use lowlat_sim::relay::{Counts, Relay};
use lowlat_sim::stream::Segment;
use lowlat_sim::{Link, Sim};

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

/// A relay attempt to `server` over a stream.
fn over_stream(server: SocketAddr) -> Config {
    Config {
        relay: Some(RelayConfig::new(server, "user", "password").over(Carrier::Stream)),
        ..Config::default()
    }
}

/// The client's connection, if it comes within ten seconds: a client that
/// never connects fails the test rather than holding it forever.
fn accept_within(listener: &TcpListener) -> Option<TcpStream> {
    listener.set_nonblocking(true).unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                return Some(socket);
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
    None
}

/// The simulator's relay behind a listener on loopback, until its client's
/// stream is over or `hang_up` is set, when it closes the connection. What
/// the relay did comes back.
fn relay_behind(listener: TcpListener, hang_up: Arc<AtomicBool>) -> thread::JoinHandle<Counts> {
    thread::spawn(move || {
        let mut socket = accept_within(&listener).expect("the client never connected");
        socket.set_nodelay(true).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(5)))
            .unwrap();
        // No delay: what the client writes is answered as it arrives.
        let mut sim = Sim::new(1).with_link(Link {
            one_way_ms: 0.0,
            ..Link::default()
        });
        let at = |last| SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 3478);
        let client = sim.add_host(at(10), &[]);
        let mut relay = Relay::new(&mut sim, at(50), at(50).ip(), &[], "user", "password");
        let stream = sim.connect(client, at(50)).expect("the relay listens");
        let mut buf = vec![0u8; 64 * 1024];
        let mut over = false;
        loop {
            if hang_up.load(Ordering::Relaxed) {
                return relay.counts.clone();
            }
            match socket.read(&mut buf) {
                Ok(0) => {
                    sim.close(stream, client);
                    over = true;
                }
                Ok(read) => sim.stream_send(stream, client, &buf[..read]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => {
                    sim.close(stream, client);
                    over = true;
                }
            }
            while let Some(arrival) = sim.next_stream_arrival() {
                if arrival.host != client {
                    relay.receive_stream(&mut sim, &arrival);
                    continue;
                }
                match arrival.data {
                    Segment::Bytes(bytes) => {
                        let _ = socket.write_all(&bytes);
                    }
                    Segment::End => return relay.counts.clone(),
                }
            }
            if over {
                return relay.counts.clone();
            }
        }
    })
}

fn listener() -> (TcpListener, SocketAddr) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    (listener, addr)
}

/// Events until `done` says so; panics past ten seconds.
fn until(one: &mut Client, mut done: impl FnMut(&Event) -> bool) -> Vec<Event> {
    let end = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    loop {
        while let Some(received) = one.poll_event() {
            let finished = done(&received.event);
            seen.push(received.event);
            if finished {
                return seen;
            }
        }
        assert!(Instant::now() < end, "never came; seen: {seen:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

fn ended(event: &Event) -> Option<Outcome> {
    match event {
        Event::Ended { outcome } => Some(*outcome),
        _ => None,
    }
}

/// A connect refused is the relay unreachable at once, not at the setup's
/// deadline.
#[test]
fn a_refused_connect_is_the_relay_unreachable() {
    let (listener, addr) = listener();
    drop(listener);
    let mut one = client();
    one.new_attempt("a", over_stream(addr), Transport::Bud)
        .expect("credentials");
    let started = Instant::now();
    one.begin_p2p("a", &peer()).expect("the attempt began");
    let seen = until(&mut one, |event| ended(event).is_some());
    assert_eq!(seen.last().and_then(ended), Some(Outcome::RelayUnreachable));
    assert!(
        started.elapsed() < Duration::from_millis(4_500),
        "ended at {} ms, the deadline rather than the refusal",
        started.elapsed().as_millis()
    );
}

/// A stream that ends before the relay has allocated is the relay
/// unreachable.
#[test]
fn a_stream_that_ends_during_setup_is_the_relay_unreachable() {
    let (listener, addr) = listener();
    let refuse = thread::spawn(move || drop(accept_within(&listener)));
    let mut one = client();
    one.new_attempt("a", over_stream(addr), Transport::Bud)
        .expect("credentials");
    one.begin_p2p("a", &peer()).expect("the attempt began");
    let seen = until(&mut one, |event| ended(event).is_some());
    refuse.join().unwrap();
    assert_eq!(seen.last().and_then(ended), Some(Outcome::RelayUnreachable));
}

/// Once the relay is ready -- its relayed address offered, then the
/// readiness marker -- a stream that ends is the relay lost. Nothing went to
/// the relay's address over UDP meanwhile: the attempt has no datagram socket.
#[test]
fn a_stream_that_ends_once_ready_is_the_relay_lost() {
    let (listener, addr) = listener();
    let datagrams = UdpSocket::bind(addr).expect("the relay's port, for datagrams");
    datagrams
        .set_read_timeout(Some(Duration::from_millis(1)))
        .unwrap();
    let hang_up = Arc::new(AtomicBool::new(false));
    let relay = relay_behind(listener, Arc::clone(&hang_up));
    let mut one = client();
    one.new_attempt("a", over_stream(addr), Transport::Bud)
        .expect("credentials");
    one.begin_p2p("a", &peer()).expect("the attempt began");

    let seen = until(&mut one, |event| matches!(event, Event::Ready));
    assert!(
        seen.iter().any(|event| matches!(
            event,
            Event::Candidate {
                from_stun: true,
                ..
            }
        )),
        "the relayed address was not offered: {seen:?}"
    );
    hang_up.store(true, Ordering::Relaxed);
    let counts = relay.join().unwrap();
    assert_eq!(counts.allocations, 1);
    let seen = until(&mut one, |event| ended(event).is_some());
    assert_eq!(seen.last().and_then(ended), Some(Outcome::RelayLost));
    let mut buf = [0u8; 64];
    assert!(
        datagrams.recv_from(&mut buf).is_err(),
        "a datagram reached the relay's address"
    );
}

/// The clean leave writes the allocation's release before it closes the
/// stream: the relay frees the allocation on the release, not on the close.
#[test]
fn the_clean_leave_writes_the_release_before_closing() {
    let (listener, addr) = listener();
    let relay = relay_behind(listener, Arc::new(AtomicBool::new(false)));
    let mut one = client();
    one.new_attempt("a", over_stream(addr), Transport::Bud)
        .expect("credentials");
    one.begin_p2p("a", &peer()).expect("the attempt began");
    until(&mut one, |event| matches!(event, Event::Ready));
    one.end_connection("a");
    let counts = relay.join().unwrap();
    assert_eq!(counts.releases, 1, "no release arrived: {counts:?}");
    assert_eq!(
        counts.closed, 0,
        "the close freed the allocation: {counts:?}"
    );
}
