//! The translation in front of a socket, asked of reflexive servers.
//!
//! The core's probe on a socket nothing else is reading: requests out at its
//! cadence, answers in, until every server has said where it sees the socket,
//! the deadline has passed, or the caller stops it. The socket is never given
//! to a loop for this, so a socket an attempt is lent afterwards is the same
//! one, as it was.

use core::net::{IpAddr, SocketAddr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::io;

use lowlat_core::nat::{self, Answer, Probe, Verdict};

use crate::addrs::local_toward;
use crate::socket::{RECV_SLOT, Socket};

/// The longest a wait lasts before the stop is looked at again.
const SLICE: Duration = Duration::from_millis(50);

/// What a probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probed {
    pub verdict: Verdict,
    /// Servers asked, and how many of them answered.
    pub asked: usize,
    pub answered: usize,
}

/// Ask `servers` from `socket` where they see it, until each has answered,
/// `deadline` has passed, or `stop` is set; then say what the answers mean.
/// Four servers are asked at most. `seed` derives the transaction
/// identifiers, and must be unpredictable: they are what admits an answer.
///
/// **Nothing on the network is an error.** A request the network refuses is
/// a server that does not answer, and a receive that fails is waited out; an
/// error is the socket's own, its address unreadable.
pub fn probe(
    socket: &Socket,
    servers: &[SocketAddrV4],
    seed: [u8; 16],
    deadline: Duration,
    stop: &AtomicBool,
) -> io::Result<Probed> {
    let mut probe = Probe::new(seed);
    for server in servers {
        // Past the fourth, refused and not asked.
        let _ = probe.add_server(SocketAddr::V4(*server));
    }
    let began = lowlat_common::clock::Time::now();
    let limit_ms = deadline.as_secs_f64() * 1000.0;
    let mut request = [0u8; lowlat_core::stun::MAX_BUILT];
    let mut arrived = vec![0u8; RECV_SLOT];
    loop {
        let now = lowlat_common::clock::elapsed_ms(began);
        // Acquire: whatever the stopping thread wrote before it is seen.
        if probe.done() || now >= limit_ms || stop.load(Ordering::Acquire) {
            break;
        }
        while let Some(Ok((to, len))) = probe.get_output(now, &mut request) {
            let _ = socket.send_to(request.get(..len).unwrap_or_default(), to);
        }
        let due_ms = probe.next_timer_ms(now).min(limit_ms - now).max(1.0);
        let mut wait = Duration::from_secs_f64(due_ms / 1000.0).min(SLICE);
        // Everything that has arrived, then the next request's time.
        loop {
            match socket.recv_from(&mut arrived, wait) {
                Ok(Some((len, from))) => {
                    let _ = probe.process_input(arrived.get(..len).unwrap_or_default(), from);
                    wait = Duration::ZERO;
                }
                Ok(None) => break,
                Err(_) => {
                    std::thread::sleep(wait);
                    break;
                }
            }
        }
    }
    let answers: Vec<Answer> = probe.answers().collect();
    // The socket as the servers would see it untranslated: the address it
    // sends from toward the first to answer, at its own port.
    let port = socket.local_addr()?.port();
    let local = answers.first().and_then(|first| match first.server {
        SocketAddr::V4(server) => {
            local_toward(server).map(|ip| SocketAddr::new(IpAddr::V4(ip), port))
        }
        SocketAddr::V6(_) => None,
    });
    Ok(Probed {
        verdict: nat::classify(local, &answers),
        asked: probe.asked(),
        answered: answers.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::Ipv4Addr;
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::thread;

    use lowlat_core::stun::{self, Message};

    /// A reflexive server on loopback that reports the source it saw, moved
    /// by `shift` ports -- a translator in a few lines -- and answers `limit`
    /// requests before it falls silent.
    fn server(
        at: Ipv4Addr,
        shift: u16,
        limit: usize,
    ) -> Option<(SocketAddrV4, thread::JoinHandle<()>)> {
        let socket = UdpSocket::bind(SocketAddrV4::new(at, 0)).ok()?;
        socket.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
        let SocketAddr::V4(addr) = socket.local_addr().ok()? else {
            return None;
        };
        let thread = thread::spawn(move || {
            let mut buf = [0u8; 256];
            for _ in 0..limit {
                let Ok((len, from)) = socket.recv_from(&mut buf) else {
                    return;
                };
                let Ok(request) = Message::parse(&buf[..len]) else {
                    continue;
                };
                let seen = SocketAddr::new(from.ip(), from.port().wrapping_add(shift));
                let mut out = [0u8; 256];
                let len =
                    stun::encode_binding_response(&mut out, request.transaction_id(), seen, "any")
                        .unwrap();
                let _ = socket.send_to(&out[..len], from);
            }
        });
        Some((addr, thread))
    }

    /// Distinct per call, which is all a test's identifiers need.
    fn seed() -> [u8; 16] {
        use std::sync::atomic::AtomicU8;
        static NEXT: AtomicU8 = AtomicU8::new(1);
        [NEXT.fetch_add(1, Ordering::Relaxed); 16]
    }

    /// Two servers at two addresses that see the socket alike: one public
    /// port for both, nothing translating -- the socket's own address -- and
    /// both asked and answered.
    #[test]
    fn two_servers_that_agree_on_the_socket_itself_are_no_translation() {
        let (Some((a, first)), Some((b, second))) = (
            server(Ipv4Addr::LOCALHOST, 0, 1),
            server(Ipv4Addr::new(127, 0, 0, 2), 0, 1),
        ) else {
            return;
        };
        let socket = Socket::open(0).unwrap();
        let stop = AtomicBool::new(false);
        let probed = probe(&socket, &[a, b], seed(), Duration::from_secs(3), &stop).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!((probed.asked, probed.answered), (2, 2));
        assert_eq!(probed.verdict.mapping, nat::Mapping::None);
        assert!(probed.verdict.port_preserved);
    }

    /// A translator that gives each destination its own port, seen by two
    /// servers: a port per destination, and the first one's is the public
    /// address.
    #[test]
    fn two_servers_that_see_two_ports_are_a_port_per_destination() {
        let (Some((a, first)), Some((b, second))) = (
            server(Ipv4Addr::LOCALHOST, 1, 1),
            server(Ipv4Addr::new(127, 0, 0, 2), 2, 1),
        ) else {
            return;
        };
        let socket = Socket::open(0).unwrap();
        let port = socket.local_addr().unwrap().port();
        let stop = AtomicBool::new(false);
        let probed = probe(&socket, &[a, b], seed(), Duration::from_secs(3), &stop).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        assert_eq!(probed.verdict.mapping, nat::Mapping::Dependent);
        assert_eq!(
            probed.verdict.public,
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                port.wrapping_add(1)
            ))
        );
        assert!(!probed.verdict.port_preserved);
    }

    /// A server that never answers is waited for until the deadline and no
    /// longer; a stop ends the wait at once.
    #[test]
    fn silence_ends_at_the_deadline_and_a_stop_ends_it_sooner() {
        let Some((silent, _thread)) = server(Ipv4Addr::LOCALHOST, 0, 0) else {
            return;
        };
        let socket = Socket::open(0).unwrap();
        let stop = AtomicBool::new(false);
        let began = std::time::Instant::now();
        let probed = probe(
            &socket,
            &[silent],
            seed(),
            Duration::from_millis(700),
            &stop,
        )
        .unwrap();
        let took = began.elapsed();
        assert!(
            took >= Duration::from_millis(700) && took < Duration::from_millis(1200),
            "{took:?}"
        );
        assert_eq!((probed.asked, probed.answered), (1, 0));
        assert_eq!(probed.verdict.mapping, nat::Mapping::NoAnswer);

        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let stopper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            stopping.store(true, Ordering::Release);
        });
        let began = std::time::Instant::now();
        probe(&socket, &[silent], seed(), Duration::from_secs(10), &stop).unwrap();
        stopper.join().unwrap();
        assert!(
            began.elapsed() < Duration::from_millis(400),
            "{:?}",
            began.elapsed()
        );
    }
}
