//! The media socket: one descriptor carrying connectivity checks and media.
//!
//! Every option is set once at open. **Nothing here lowers one afterwards**,
//! with the single exception of the mapping probe's TTL, which is raised back
//! in the same call that lowered it. A setup path that shrank a receive buffer
//! and left it shrunk has already cost a production stream.
//!
//! This is the first module in the workspace outside the concurrency primitives
//! to contain `unsafe`, because batched receive is a syscall. Every block is a
//! thin wrapper whose safety argument is local and stated. `miri` cannot reach
//! any of it -- it cannot execute a syscall -- so the sanitizer build carries
//! that weight instead.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::time::Duration;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};

use lowlat_core::MAX_DATAGRAM;

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
const WANT_RCVBUF: libc::c_int = 64 * 1024 * 1024;

/// Requested send buffer. The default drops check and video bursts.
const WANT_SNDBUF: libc::c_int = 4 * 1024 * 1024;

/// Expedited forwarding, on both families.
const DSCP_EF: libc::c_int = 0xB8;

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

/// Sizes into the kernel's length type.
///
/// Every value passed is a compile-time struct size, orders of magnitude below
/// the type's range. The saturating fallback keeps the conversion total rather
/// than panicking on a case that cannot arise.
pub(crate) fn socklen(bytes: usize) -> libc::socklen_t {
    libc::socklen_t::try_from(bytes).unwrap_or(libc::socklen_t::MAX)
}

/// The address family, in the type the kernel's address structs use.
fn family_v6() -> libc::sa_family_t {
    libc::sa_family_t::try_from(libc::AF_INET6).unwrap_or(0)
}

/// A bound UDP socket with the full option set applied.
#[derive(Debug)]
pub struct Socket {
    fd: OwnedFd,
    granted_rcvbuf: i32,
    granted_sndbuf: i32,
}

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

    /// One attempt: a fresh descriptor, the full option set, and a bind.
    fn bound(port: u16) -> io::Result<Self> {
        // SAFETY: a plain socket(2) with constant arguments; the returned
        // descriptor is handed straight to OwnedFd, which closes it on drop.
        let raw = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor we own and have not registered
        // anywhere else.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let socket = Self {
            fd,
            granted_rcvbuf: 0,
            granted_sndbuf: 0,
        };
        socket.configure()?;
        socket.bind(port)?;

        let granted_rcvbuf = socket.get_int(libc::SOL_SOCKET, libc::SO_RCVBUF)?;
        let granted_sndbuf = socket.get_int(libc::SOL_SOCKET, libc::SO_SNDBUF)?;
        Ok(Self {
            granted_rcvbuf,
            granted_sndbuf,
            ..socket
        })
    }

    fn configure(&self) -> io::Result<()> {
        // Dual stack. One socket serves both families.
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 0)?;

        // Ask high and accept what the kernel grants; the granted value is
        // reported by `granted_recv_buffer` and must be logged at open, because
        // a silently clamped request is invisible until a burst is lost.
        self.set_int(libc::SOL_SOCKET, libc::SO_RCVBUF, WANT_RCVBUF)?;
        self.set_int(libc::SOL_SOCKET, libc::SO_SNDBUF, WANT_SNDBUF)?;

        // Source address selection parity across families.
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO, 1)?;
        self.set_int(libc::IPPROTO_IP, libc::IP_PKTINFO, 1)?;

        self.set_int(libc::IPPROTO_IP, libc::IP_TOS, DSCP_EF)?;
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, DSCP_EF)?;

        // Do not fragment, so an oversized probe fails fast rather than being
        // split and arriving anyway, which would make the probe meaningless.
        self.set_int(
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            libc::IP_PMTUDISC_DO,
        )?;

        self.set_ttl(DEFAULT_TTL)?;
        self.set_nonblocking()?;
        Ok(())
    }

    fn bind(&self, port: u16) -> io::Result<()> {
        let addr = libc::sockaddr_in6 {
            sin6_family: family_v6(),
            sin6_port: port.to_be(),
            sin6_flowinfo: 0,
            sin6_addr: libc::in6_addr { s6_addr: [0u8; 16] },
            sin6_scope_id: 0,
        };
        // SAFETY: `addr` is a fully initialised sockaddr_in6 and the length
        // passed is its exact size.
        let rc = unsafe {
            libc::bind(
                self.fd.as_raw_fd(),
                core::ptr::addr_of!(addr).cast(),
                socklen(mem::size_of::<libc::sockaddr_in6>()),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The receive buffer the kernel actually granted.
    ///
    /// **Log this at open, every time.** A clamped request is otherwise
    /// invisible until a keyframe burst is already lost.
    pub fn granted_recv_buffer(&self) -> i32 {
        self.granted_rcvbuf
    }

    /// The send buffer the kernel actually granted.
    pub fn granted_send_buffer(&self) -> i32 {
        self.granted_sndbuf
    }

    /// The address the socket is bound to, after the kernel has chosen a port.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = socklen(mem::size_of::<libc::sockaddr_storage>());
        // SAFETY: `storage` is large enough for any address family and `len`
        // describes it exactly; the kernel writes at most that many bytes.
        let rc = unsafe {
            libc::getsockname(
                self.fd.as_raw_fd(),
                core::ptr::addr_of_mut!(storage).cast(),
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        from_storage(&storage).ok_or_else(|| io::Error::other("unrecognised local address"))
    }

    /// Set the hop limit on both families.
    ///
    /// Lowering this for a mapping probe is the one option change permitted
    /// after open, and it must be raised again in the same breath. A socket
    /// left at a probe TTL carries media a few hops and no further, which
    /// presents as a path that establishes and then delivers nothing.
    pub fn set_ttl(&self, ttl: u8) -> io::Result<()> {
        self.set_int(libc::IPPROTO_IP, libc::IP_TTL, i32::from(ttl))?;
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_UNICAST_HOPS, i32::from(ttl))
    }

    /// The current hop limit, read back from the kernel.
    pub fn ttl(&self) -> io::Result<u8> {
        let value = self.get_int(libc::IPPROTO_IP, libc::IP_TTL)?;
        u8::try_from(value).map_err(|_| io::Error::other("ttl outside the byte range"))
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        // SAFETY: F_GETFL takes no argument and returns the flags or -1.
        let flags = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: F_SETFL takes the flag word by value.
        let rc =
            unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn set_int(&self, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
        // SAFETY: the option value is a c_int and the length passed is its
        // exact size, which is what every option used here expects.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                level,
                name,
                core::ptr::addr_of!(value).cast(),
                socklen(mem::size_of::<libc::c_int>()),
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn get_int(&self, level: libc::c_int, name: libc::c_int) -> io::Result<i32> {
        let mut value: libc::c_int = 0;
        let mut len = socklen(mem::size_of::<libc::c_int>());
        // SAFETY: `value` is a c_int and `len` describes it exactly; the kernel
        // writes at most that many bytes and updates `len`.
        let rc = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                level,
                name,
                core::ptr::addr_of_mut!(value).cast(),
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(value)
    }

    /// Send one datagram.
    pub fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<usize> {
        let (addr, len) = to_storage(to);
        // SAFETY: `datagram` is a valid slice and `addr` a fully initialised
        // address of exactly `len` bytes.
        let sent = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                datagram.as_ptr().cast(),
                datagram.len(),
                0,
                core::ptr::addr_of!(addr).cast(),
                len,
            )
        };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(usize::try_from(sent).unwrap_or(0))
    }

    /// Wait until the socket is readable, or `timeout_ms` elapses.
    ///
    /// Returns true if there is something to read. A sub-millisecond timeout
    /// rounds up to one, because a wait that can return instantly is a hot poll
    /// wearing a timeout's clothes.
    pub fn wait_readable(&self, timeout_ms: f64) -> io::Result<bool> {
        let timeout = if timeout_ms <= 1.0 {
            1
        } else if timeout_ms >= f64::from(i32::MAX) {
            i32::MAX
        } else {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "bounded by the two arms above"
            )]
            {
                timeout_ms as i32
            }
        };
        let mut fds = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one fully initialised pollfd is passed with a count of one.
        let rc = unsafe { libc::poll(core::ptr::addr_of_mut!(fds), 1, timeout) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(rc > 0)
    }

    /// Give up ownership of the descriptor.
    pub fn into_raw_fd(self) -> RawFd {
        self.fd.into_raw_fd()
    }
}

impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Convert a socket address into the kernel's form.
pub(crate) fn to_storage(addr: SocketAddr) -> (libc::sockaddr_in6, libc::socklen_t) {
    // The socket is dual stack, so a v4 destination goes out as v4-mapped.
    let (ip, port) = match addr {
        SocketAddr::V4(v4) => (v4.ip().to_ipv6_mapped(), v4.port()),
        SocketAddr::V6(v6) => (*v6.ip(), v6.port()),
    };
    let storage = libc::sockaddr_in6 {
        sin6_family: family_v6(),
        sin6_port: port.to_be(),
        sin6_flowinfo: 0,
        sin6_addr: libc::in6_addr {
            s6_addr: ip.octets(),
        },
        sin6_scope_id: 0,
    };
    (storage, socklen(mem::size_of::<libc::sockaddr_in6>()))
}

/// Convert the kernel's form back, collapsing a v4-mapped address to IPv4.
///
/// **Structural, never textual.** A v4-mapped address contains colons in its
/// text form and is IPv4; deciding by searching for one removes every v4
/// candidate and kills connectivity on v4-only paths.
pub(crate) fn from_storage(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    match libc::c_int::from(storage.ss_family) {
        libc::AF_INET6 => {
            // SAFETY: the family field says this is a sockaddr_in6, and
            // sockaddr_storage is defined to be large enough and aligned for it.
            let v6 = unsafe { &*core::ptr::from_ref(storage).cast::<libc::sockaddr_in6>() };
            let ip = Ipv6Addr::from(v6.sin6_addr.s6_addr);
            let port = u16::from_be(v6.sin6_port);
            Some(match ip.to_ipv4_mapped() {
                Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, port)),
                None => SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    port,
                    v6.sin6_flowinfo,
                    v6.sin6_scope_id,
                )),
            })
        }
        libc::AF_INET => {
            // SAFETY: as above, for the v4 layout.
            let v4 = unsafe { &*core::ptr::from_ref(storage).cast::<libc::sockaddr_in>() };
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))),
                u16::from_be(v4.sin_port),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let right = Socket::open(0).expect("open right");

        let mut to = right.local_addr().expect("addr");
        to.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        left.send_to(b"hello", to).expect("send");

        assert!(right.wait_readable(500.0).expect("poll"), "nothing arrived");
    }

    /// A v4-mapped address is IPv4 and must come back as such, whatever the
    /// dual-stack socket hands us.
    #[test]
    fn a_v4_mapped_source_is_reported_as_v4() {
        let mapped = Ipv4Addr::new(198, 51, 100, 7).to_ipv6_mapped();
        let (storage, _) = to_storage(SocketAddr::new(IpAddr::V6(mapped), 9000));

        // SAFETY: reinterpreting a sockaddr_in6 as the storage union is exactly
        // what the kernel interface does, and the storage is the larger type.
        let generic: libc::sockaddr_storage = unsafe {
            let mut out: libc::sockaddr_storage = mem::zeroed();
            core::ptr::copy_nonoverlapping(
                core::ptr::addr_of!(storage).cast::<u8>(),
                core::ptr::addr_of_mut!(out).cast::<u8>(),
                mem::size_of::<libc::sockaddr_in6>(),
            );
            out
        };

        assert_eq!(
            from_storage(&generic),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)),
                9000
            ))
        );
    }
}
