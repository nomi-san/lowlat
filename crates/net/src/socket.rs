//! The media socket: one descriptor carrying connectivity checks and media.
//!
//! Every option is set once at open. **Nothing here lowers one afterwards**,
//! with the single exception of the mapping probe's TTL, which is raised back
//! in the same call that lowered it. A setup path that shrank a receive buffer
//! and left it shrunk has already cost a production stream.
//!
//! The socket itself and its option calls are the platform's
//! (`crate::sys`); the sizes, the values asked for and the port walk are here.

use core::time::Duration;
use std::io;

use lowlat_core::MAX_DATAGRAM;

pub use crate::sys::Socket;

/// Receive slot size, derived from the protocol ceiling and never from the
/// probed datagram size.
///
/// Sizing a receive buffer from the negotiated or probed size silently discards
/// whole datagrams and presents as "control works, video does not". The probed
/// size and this are different quantities and must never share an identifier.
pub const RECV_SLOT: usize = MAX_DATAGRAM + RELAY_MARGIN;

/// What a relay adds ahead of our datagram.
const RELAY_MARGIN: usize = 64;

// The slot is derived from the protocol ceiling, so make it impossible to
// redefine it as the probed datagram size by editing one constant. Conflating
// the two silently discards full-size datagrams while small ones pass.
const _: () = assert!(RECV_SLOT > MAX_DATAGRAM);
const _: () = assert!(RECV_SLOT > lowlat_core::DEFAULT_DATAGRAM);

/// Datagrams pulled from the kernel per syscall.
///
/// A single outstanding receive plus a poll loses a keyframe burst outright.
pub const RECV_BATCH: usize = 64;

/// Requested receive buffer. Keyframe bursts of roughly 2550 packets per 100 ms
/// overflow 16 MB, so this asks far above what most kernels will grant and logs
/// what actually arrived.
pub(crate) const WANT_RCVBUF: i32 = 64 * 1024 * 1024;

/// Requested send buffer. The default drops check and video bursts.
pub(crate) const WANT_SNDBUF: i32 = 4 * 1024 * 1024;

/// Expedited forwarding, on both families. A socket option where the system
/// honours one; Windows ignores it and marks per destination instead.
#[cfg(target_os = "linux")]
pub(crate) const DSCP_EF: i32 = 0xB8;

/// The TTL everything but a mapping probe leaves at.
pub const DEFAULT_TTL: u8 = 64;

/// Hop limit used for a mapping probe.
///
/// High enough to open the mapping on the way out of the local network, far too
/// low to reach the peer. Mirrors the core's probe value.
pub const PROBE_TTL_MAX: u8 = lowlat_core::conn::PROBE_TTL;

/// Ports tried, starting at the requested one, before the walk gives up.
///
/// A host whose configured port is occupied has to start anyway, so the bind
/// steps forward rather than failing. Each attempt takes a fresh descriptor:
/// the option set is applied before the bind, so a failed bind cannot be
/// retried on the socket that carried it.
///
/// The walk stops at the top of the range instead of wrapping. Wrapping lands
/// on the privileged ports, where the bind fails for an unrelated reason and
/// reports it as though the range were occupied.
const PORT_WALK: u16 = 50;

/// Between walk attempts.
///
/// A failed bind says nothing about the next port, so this is not waiting for
/// anything to clear. It bounds the pathological case where the failure is
/// descriptor exhaustion rather than an occupied port, and the whole walk is
/// still under the time a single connection setup takes.
const WALK_RETRY: Duration = Duration::from_millis(1);

impl Socket {
    /// Open a dual-stack UDP socket bound at or just above `port`, with every
    /// option set.
    ///
    /// Port 0 asks the kernel to choose and binds once, because there is
    /// nothing to walk. Any other port is the head of a [`PORT_WALK`] range:
    /// an occupied port steps forward rather than failing the host's start.
    /// Exhausting the range returns the last bind error, so a caller sees
    /// `AddrInUse` rather than a socket on a port it never asked for. A caller
    /// that would rather have any port than none asks for
    /// [`Socket::open_or_any_port`] by name.
    ///
    /// The socket is dual stack, so one descriptor serves both families and a
    /// v4 peer arrives as a v4-mapped address, which callers must classify
    /// structurally rather than by looking for a colon.
    pub fn open(port: u16) -> io::Result<Self> {
        Self::walk(port, false)
    }

    /// As [`Socket::open`], but take a kernel-chosen port when the walk is
    /// exhausted rather than failing.
    ///
    /// **The bound port is then not the requested one, and the caller must read
    /// it back from [`Socket::local_addr`] before advertising anything.** This
    /// is why the fallback is a separate call instead of the default: a host
    /// that silently lands on an arbitrary port advertises the port it wanted
    /// and receives nothing on it, which presents as a peer that answers checks
    /// and never establishes.
    pub fn open_or_any_port(port: u16) -> io::Result<Self> {
        Self::walk(port, true)
    }

    fn walk(port: u16, any_on_exhaustion: bool) -> io::Result<Self> {
        // Port 0 is already a request for whatever the kernel has free.
        if port == 0 {
            return Self::bound(0);
        }

        let mut last = None;
        for step in 0..PORT_WALK {
            let Some(candidate) = port.checked_add(step) else {
                break;
            };
            match Self::bound(candidate) {
                Ok(socket) => {
                    if candidate != port {
                        lowlat_common::log_info!(
                            "net: port walked, want={} bound={}",
                            port,
                            candidate
                        );
                    }
                    return Ok(socket);
                }
                Err(error) => last = Some(error),
            }
            lowlat_common::clock::precise_sleep(WALK_RETRY);
        }

        if any_on_exhaustion {
            let socket = Self::bound(0)?;
            let bound = socket.local_addr()?.port();
            lowlat_common::log_info!(
                "net: port walk exhausted, want={} bound={} ephemeral=1",
                port,
                bound
            );
            return Ok(socket);
        }

        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AddrInUse)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::Io;
    use crate::wake::Wake;
    use core::net::{IpAddr, Ipv6Addr, SocketAddr};

    fn loopback_of(socket: &Socket) -> SocketAddr {
        let mut addr = socket.local_addr().expect("addr");
        addr.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        addr
    }

    /// A socket to receive on, with the loop's own receive path around it.
    fn receiver() -> (Io, SocketAddr) {
        let socket = Socket::open(0).expect("open receiver");
        let to = loopback_of(&socket);
        (Io::new(socket, Wake::new().expect("wake")), to)
    }

    /// Wait for the socket to speak, then take one batch.
    fn arrived(io: &mut Io) -> usize {
        assert!(io.wait(1000.0).expect("wait").socket, "nothing arrived");
        io.drain().expect("drain")
    }

    #[test]
    fn a_socket_opens_with_its_options_applied() {
        let socket = Socket::open(0).expect("open");

        assert!(
            socket.granted_recv_buffer() > 0,
            "the granted receive buffer must be readable at open"
        );
        assert!(socket.granted_send_buffer() > 0);
        assert_eq!(socket.ttl().expect("ttl"), DEFAULT_TTL);

        let local = socket.local_addr().expect("local addr");
        assert_ne!(local.port(), 0, "the kernel must have chosen a port");
    }

    /// A host whose configured port is occupied has to start anyway, so the
    /// bind steps forward. Without the walk this is a hard failure at startup.
    #[test]
    fn an_occupied_port_walks_forward() {
        let occupied = Socket::open(0).expect("occupy");
        let taken = occupied.local_addr().expect("addr").port();

        let walked = Socket::open(taken).expect("the walk must find room");
        let bound = walked.local_addr().expect("addr").port();

        assert_ne!(bound, taken, "the walk bound the port it was told was busy");
        assert!(
            bound > taken && bound < taken.saturating_add(PORT_WALK),
            "walked out of its range: want={taken} bound={bound}"
        );
    }

    /// The top of the range has no successor, so the walk gives up there.
    ///
    /// Two behaviours in one fixture because they need the same exclusive port.
    /// It must report the bind failure rather than quietly landing elsewhere,
    /// and it must not wrap into the privileged ports looking for room -- a
    /// wrapping walk would answer this with a success on some low port.
    #[test]
    fn an_exhausted_walk_fails_unless_any_port_was_asked_for() {
        const TOP: u16 = u16::MAX;
        let _occupied = Socket::open(TOP);

        let error = Socket::open(TOP).expect_err("the walk cannot step past the top of the range");
        assert_eq!(
            error.kind(),
            io::ErrorKind::AddrInUse,
            "an exhausted walk must report the bind failure"
        );

        let fallback = Socket::open_or_any_port(TOP).expect("fallback");
        let bound = fallback.local_addr().expect("addr").port();
        assert_ne!(bound, 0, "the kernel must have chosen a port");
        assert_ne!(
            bound, TOP,
            "the fallback must report the port it got, not the one it wanted"
        );
    }

    /// The receive slot comes from the protocol ceiling plus relay framing, and
    /// never from the probed datagram size. Conflating the two silently
    /// discards full-size datagrams while small ones pass.
    #[test]
    fn the_receive_slot_is_not_the_datagram_size() {
        // The ordering is asserted at compile time beside the constants; this
        // pins the relay margin, which is the part a refactor could drop.
        assert_eq!(RECV_SLOT, MAX_DATAGRAM + 64);
    }

    /// The regression for the probe restore, at the layer that actually holds
    /// the socket option.
    #[test]
    fn a_lowered_ttl_is_restored() {
        let socket = Socket::open(0).expect("open");
        assert_eq!(socket.ttl().expect("ttl"), DEFAULT_TTL);

        socket.set_ttl(4).expect("lower");
        assert_eq!(socket.ttl().expect("ttl"), 4);

        socket.set_ttl(DEFAULT_TTL).expect("restore");
        assert_eq!(
            socket.ttl().expect("ttl"),
            DEFAULT_TTL,
            "a probe must not leave the socket at its TTL"
        );
    }

    #[test]
    fn a_datagram_crosses_between_two_sockets() {
        let left = Socket::open(0).expect("open left");
        let (mut right, to) = receiver();
        left.send_to(b"hello", to).expect("send");
        assert_eq!(arrived(&mut right), 1);
    }

    #[test]
    fn an_empty_socket_drains_to_nothing() {
        let (mut io, _) = receiver();
        assert_eq!(io.drain().expect("drain"), 0);
        assert_eq!(io.iter().count(), 0);
        assert!(!io.saturated());
    }

    /// The property the batch exists for: a burst arrives whole, in one call,
    /// rather than one datagram per syscall with the rest dropped.
    #[test]
    fn a_burst_arrives_in_one_call() {
        let sender = Socket::open(0).expect("open sender");
        let (mut receiver, to) = receiver();

        let burst = 32;
        for index in 0..burst {
            let payload = [index as u8; 200];
            sender.send_to(&payload, to).expect("send");
        }

        // Linux's batched receive takes everything queued in one call, which
        // is the batch's whole property. Windows completes each receive on
        // its own and a wait returns with what has completed so far -- under
        // load, measured, 17 to 25 of these 32 -- so there the burst is
        // gathered, and the pool's own tests hold the batching.
        #[cfg(target_os = "linux")]
        let seen: Vec<Vec<u8>> = {
            assert_eq!(
                arrived(&mut receiver),
                burst,
                "the burst did not arrive in one call"
            );
            receiver
                .iter()
                .map(|(_, _, bytes)| bytes.to_vec())
                .collect()
        };
        #[cfg(windows)]
        let seen: Vec<Vec<u8>> = {
            let mut seen = Vec::new();
            let started = std::time::Instant::now();
            while seen.len() < burst && started.elapsed() < Duration::from_secs(1) {
                if receiver.wait(100.0).expect("wait").socket {
                    receiver.drain().expect("drain");
                    seen.extend(receiver.iter().map(|(_, _, bytes)| bytes.to_vec()));
                }
            }
            seen
        };

        assert_eq!(seen.len(), burst, "the burst did not arrive whole");
        for (index, bytes) in seen.iter().enumerate() {
            assert_eq!(bytes.len(), 200);
            assert_eq!(bytes[0], index as u8, "datagrams arrived out of order");
        }
    }

    /// Reusing the batch must not carry the previous pass's address length
    /// forward, which is what an unreset descriptor does.
    #[test]
    fn a_reused_batch_reports_the_right_source_each_time() {
        let first = Socket::open(0).expect("open first");
        let second = Socket::open(0).expect("open second");
        let (mut receiver, to) = receiver();

        first.send_to(b"one", to).expect("send");
        assert_eq!(arrived(&mut receiver), 1);
        let (from_first, _, bytes) = receiver.iter().next().expect("one datagram");
        assert_eq!(bytes, b"one");
        assert_eq!(from_first.port(), loopback_of(&first).port());

        second.send_to(b"two", to).expect("send");
        assert_eq!(arrived(&mut receiver), 1);
        let (from_second, _, bytes) = receiver.iter().next().expect("one datagram");
        assert_eq!(bytes, b"two");
        assert_eq!(
            from_second.port(),
            loopback_of(&second).port(),
            "the second pass reported the first sender"
        );
    }

    /// The address a datagram arrived at is reported beside the address it
    /// came from. Any 127/8 destination reaches the same wildcard-bound
    /// socket, so two arrivals only differ by what the control message says
    /// -- which is exactly what a check answer needs to leave from the right
    /// address on a host that has more than one.
    #[test]
    fn the_address_a_datagram_arrived_at_is_reported() {
        let sender = Socket::open(0).expect("sender");
        let (mut receiver, to) = receiver();
        let port = to.port();

        let v4_dest = IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 11));
        sender
            .send_to(b"v4", SocketAddr::new(v4_dest, port))
            .expect("send");
        assert_eq!(arrived(&mut receiver), 1);
        let (_, local, bytes) = receiver.iter().next().expect("datagram");
        assert_eq!(bytes, b"v4");
        assert_eq!(
            local,
            Some(v4_dest),
            "the v4 arrival address was not reported"
        );

        let v6_dest = IpAddr::V6(Ipv6Addr::LOCALHOST);
        sender
            .send_to(b"v6", SocketAddr::new(v6_dest, port))
            .expect("send");
        assert_eq!(arrived(&mut receiver), 1);
        let (_, local, bytes) = receiver.iter().next().expect("datagram");
        assert_eq!(bytes, b"v6");
        assert_eq!(
            local,
            Some(v6_dest),
            "the v6 arrival address was not reported"
        );
    }

    /// **A measurement, not a gate**: how long the receive path takes to hand
    /// over a keyframe-sized burst already queued on the socket, per burst and
    /// per datagram. The burst is sent whole before the clock starts, so what
    /// is timed is the platform's receive alone, and the thread is busy for
    /// all of it, so the time is its cost. The baseline any other receive
    /// mechanism has to beat. Run with
    /// `cargo test -p lowlat-net --release --lib receive_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, run by hand in release"]
    fn receive_cost() {
        const BURST: usize = 2550;
        const SIZE: usize = 1200;
        const ROUNDS: usize = 50;
        let sender = Socket::open(0).expect("open sender");
        let (mut io, to) = receiver();
        let payload = [0x5Au8; SIZE];

        let mut per_burst = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            for _ in 0..BURST {
                while let Err(error) = sender.send_to(&payload, to) {
                    assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "send: {error}");
                    std::thread::yield_now();
                }
            }
            std::thread::sleep(Duration::from_millis(20));

            let started = std::time::Instant::now();
            let mut got = 0;
            while got < BURST && io.wait(100.0).expect("wait").socket {
                loop {
                    let drained = io.drain().expect("drain");
                    got += io
                        .iter()
                        .filter(|(_, _, bytes)| bytes.len() == SIZE)
                        .count();
                    if drained == 0 || !io.saturated() {
                        break;
                    }
                }
            }
            per_burst.push(started.elapsed().as_secs_f64() * 1e3);
            assert_eq!(got, BURST, "the burst did not arrive whole");
        }

        per_burst.sort_by(f64::total_cmp);
        let at = |p: f64| per_burst[((per_burst.len() - 1) as f64 * p).round() as usize];
        println!(
            "receive: {BURST} x {SIZE} B queued, {ROUNDS} rounds: \
             p50 {:.3} p95 {:.3} p99 {:.3} max {:.3} ms a burst; \
             p50 {:.0} ns a datagram",
            at(0.50),
            at(0.95),
            at(0.99),
            per_burst[ROUNDS - 1],
            at(0.50) * 1e6 / BURST as f64
        );
    }

    /// A full-size datagram must survive, which it only does because the slot
    /// is sized from the protocol ceiling plus relay framing.
    #[test]
    fn a_full_size_datagram_survives() {
        let sender = Socket::open(0).expect("open sender");
        let (mut receiver, to) = receiver();

        let payload = vec![0xA5u8; lowlat_core::MAX_DATAGRAM];
        sender.send_to(&payload, to).expect("send");

        assert_eq!(arrived(&mut receiver), 1);
        let (_, _, bytes) = receiver.iter().next().expect("one datagram");
        assert_eq!(
            bytes.len(),
            lowlat_core::MAX_DATAGRAM,
            "a full-size datagram was truncated"
        );
    }
}
