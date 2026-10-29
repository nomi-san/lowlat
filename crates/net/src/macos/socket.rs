//! The media socket through the system's own interface: the descriptor, the
//! option set, the bind that takes a port free in both families, and the
//! address conversions.
//!
//! **The socket is dual stack, and every v4-level option is refused on it** --
//! the type of service, the hop limit, don't-fragment, the arrival address. It
//! is set through its v6 options, which the system applies to v4 traffic where
//! it can -- the hop limit, don't-fragment, and the arrival address, which a v4
//! datagram reports v4-mapped -- and a v4 send carries its mark and its source
//! itself, as control messages (`send.rs`).

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use core::time::Duration;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::socket::{DEFAULT_TTL, DSCP_EF, WANT_RCVBUF, WANT_SNDBUF};

/// Kernel-chosen ports tried before an open that asked for any port fails.
///
/// The choice is made on the v4 side, so it is free for IPv4; a port some
/// socket holds for IPv6 alone still refuses the dual-stack bind, and the next
/// choice is almost certainly free.
const PICKS: usize = 8;

/// The smallest a refused buffer request steps down to before the refusal
/// stands.
const BUFFER_FLOOR: i32 = 256 * 1024;

/// Sizes into the system's length type.
///
/// Every value passed is a compile-time struct size, orders of magnitude below
/// the type's range. The saturating fallback keeps the conversion total rather
/// than panicking on a case that cannot arise.
pub(super) fn socklen(bytes: usize) -> libc::socklen_t {
    libc::socklen_t::try_from(bytes).unwrap_or(libc::socklen_t::MAX)
}

/// An address structure's own length, which this system's structures carry.
fn sa_len<T>() -> u8 {
    u8::try_from(mem::size_of::<T>()).unwrap_or(u8::MAX)
}

/// The address family, in the type the address structures use.
fn family(value: libc::c_int) -> libc::sa_family_t {
    libc::sa_family_t::try_from(value).unwrap_or(0)
}

/// Non-blocking, and closed in a child the host process starts: a library's
/// descriptor must not outlive it in someone else's program.
pub(super) fn set_flags(fd: RawFd) -> io::Result<()> {
    // SAFETY: F_GETFL takes no argument and returns the flags or -1.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFL takes the flag word by value.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFD takes the descriptor flags by value.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A bound UDP socket with the full option set applied.
#[derive(Debug)]
pub struct Socket {
    fd: OwnedFd,
    granted_rcvbuf: i32,
    granted_sndbuf: i32,
}

impl Socket {
    /// One attempt: the port taken free in both families, then a fresh
    /// descriptor, the full option set, and a bind.
    ///
    /// **A port another socket holds for IPv4 does not stop a dual-stack bind
    /// here**, and that socket then keeps every v4 datagram; the system's own
    /// choice for port 0 does not avoid such ports either. There is no
    /// exclusive-use option, so an IPv4 socket binds the port first --
    /// refused where the port is held for IPv4, and holding it against
    /// anyone else meanwhile -- the dual-stack socket binds the port it got,
    /// and the IPv4 one closes. Once the dual-stack socket holds a port an
    /// IPv4 bind to it is refused, so the close leaves no gap.
    pub(crate) fn bound(port: u16) -> io::Result<Self> {
        let picks = if port == 0 { PICKS } else { 1 };
        let mut last = None;
        for _ in 0..picks {
            let (held, chosen) = hold_v4(port)?;
            let attempt = Self::bound_at(chosen);
            drop(held);
            match attempt {
                Ok(socket) => return Ok(socket),
                Err(error) if error.kind() == io::ErrorKind::AddrInUse => last = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AddrInUse)))
    }

    /// The dual-stack socket itself, at a port its IPv4 side holds free.
    fn bound_at(port: u16) -> io::Result<Self> {
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

        // Ask high and accept what the system grants; the grant is reported by
        // `granted_recv_buffer` and logged at open, because a clamped request
        // is invisible until a burst is lost.
        self.ask_buffer(libc::SO_RCVBUF, WANT_RCVBUF)?;
        self.ask_buffer(libc::SO_SNDBUF, WANT_SNDBUF)?;

        // The arrival address, for both families: a v4 arrival comes v4-mapped.
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_RECVPKTINFO, 1)?;

        // The mark of every v6 send; a v4 send carries its own.
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, DSCP_EF)?;

        // Do not fragment, so an oversized probe fails fast rather than being
        // split and arriving anyway, which would make the probe meaningless.
        // The v6 option covers v4 sends too; the v4 one is refused here.
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_DONTFRAG, 1)?;

        self.set_ttl(DEFAULT_TTL)?;
        set_flags(self.fd.as_raw_fd())
    }

    /// Ask for a buffer of `want` bytes. A request above the system's limit
    /// is clamped to it here; where one is refused instead, it is asked again
    /// for three quarters until it is taken. What was granted is read back
    /// either way.
    fn ask_buffer(&self, name: libc::c_int, want: i32) -> io::Result<()> {
        let mut ask = want;
        loop {
            match self.set_int(libc::SOL_SOCKET, name, ask) {
                Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) && ask > BUFFER_FLOOR => {
                    ask = ask / 4 * 3;
                }
                result => return result,
            }
        }
    }

    fn bind(&self, port: u16) -> io::Result<()> {
        let addr = libc::sockaddr_in6 {
            sin6_len: sa_len::<libc::sockaddr_in6>(),
            sin6_family: family(libc::AF_INET6),
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

    /// The receive buffer the system actually granted.
    ///
    /// **Log this at open, every time.** A clamped request is otherwise
    /// invisible until a keyframe burst is already lost.
    pub fn granted_recv_buffer(&self) -> i32 {
        self.granted_rcvbuf
    }

    /// The send buffer the system actually granted.
    pub fn granted_send_buffer(&self) -> i32 {
        self.granted_sndbuf
    }

    /// The address the socket is bound to, after the system has chosen a port.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        // SAFETY: sockaddr_storage is plain data; all zeroes is valid.
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = socklen(mem::size_of::<libc::sockaddr_storage>());
        // SAFETY: `storage` is large enough for any address family and `len`
        // describes it exactly; the system writes at most that many bytes.
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
    ///
    /// The v6 hop limit carries to v4 sends here, so it is the one set: the
    /// v4 option is refused on this socket.
    pub fn set_ttl(&self, ttl: u8) -> io::Result<()> {
        self.set_int(libc::IPPROTO_IPV6, libc::IPV6_UNICAST_HOPS, i32::from(ttl))
    }

    /// The current hop limit, read back from the system.
    pub fn ttl(&self) -> io::Result<u8> {
        let value = self.get_int(libc::IPPROTO_IPV6, libc::IPV6_UNICAST_HOPS)?;
        u8::try_from(value).map_err(|_| io::Error::other("ttl outside the byte range"))
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
        // SAFETY: `value` is a c_int and `len` describes it exactly; the system
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

    /// Send one datagram, marked as every send from this socket is.
    pub fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<usize> {
        super::send::send(self, datagram, to, None)
    }

    /// One datagram, if one arrives within `wait`: its length, and where it
    /// came from. For a short exchange on a socket no loop has taken; a
    /// datagram longer than `buf` is cut to it.
    pub fn recv_from(
        &self,
        buf: &mut [u8],
        wait: Duration,
    ) -> io::Result<Option<(usize, SocketAddr)>> {
        let mut ready = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(wait.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one live pollfd and its count; the system writes `revents`.
        let rc = unsafe { libc::poll(&raw mut ready, 1, ms) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::Interrupted => Ok(None),
                _ => Err(error),
            };
        }
        if rc == 0 {
            return Ok(None);
        }
        // SAFETY: sockaddr_storage is plain data; all zeroes is valid.
        let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
        let mut len = socklen(mem::size_of::<libc::sockaddr_storage>());
        // SAFETY: `buf` is writable for its length, and `storage` is large
        // enough for any family with `len` describing it exactly.
        let got = unsafe {
            libc::recvfrom(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                0,
                core::ptr::addr_of_mut!(storage).cast(),
                &mut len,
            )
        };
        if got < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(error),
            };
        }
        let from = from_storage(&storage)
            .ok_or_else(|| io::Error::other("unrecognised source address"))?;
        Ok(Some((usize::try_from(got).unwrap_or(0), from)))
    }

    /// The descriptor, for the calls in this module's siblings.
    pub(super) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// An IPv4 socket bound at `port` on every address, and the port it got.
fn hold_v4(port: u16) -> io::Result<(OwnedFd, u16)> {
    // SAFETY: a plain socket(2) with constant arguments, owned at once.
    let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own and have registered nowhere.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut addr = libc::sockaddr_in {
        sin_len: sa_len::<libc::sockaddr_in>(),
        sin_family: family(libc::AF_INET),
        sin_port: port.to_be(),
        sin_addr: libc::in_addr { s_addr: 0 },
        sin_zero: [0; 8],
    };
    let mut len = socklen(mem::size_of::<libc::sockaddr_in>());
    // SAFETY: a fully initialised sockaddr_in and its exact length.
    if unsafe { libc::bind(raw, core::ptr::addr_of!(addr).cast(), len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: room for the sockaddr_in the bound socket's name is, and its
    // length, both writable.
    if unsafe { libc::getsockname(raw, core::ptr::addr_of_mut!(addr).cast(), &mut len) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fd, u16::from_be(addr.sin_port)))
}

/// Convert a socket address into the system's form.
pub(super) fn to_storage(addr: SocketAddr) -> (libc::sockaddr_in6, libc::socklen_t) {
    // The socket is dual stack, so a v4 destination goes out as v4-mapped.
    let (ip, port) = match addr {
        SocketAddr::V4(v4) => (v4.ip().to_ipv6_mapped(), v4.port()),
        SocketAddr::V6(v6) => (*v6.ip(), v6.port()),
    };
    let storage = libc::sockaddr_in6 {
        sin6_len: sa_len::<libc::sockaddr_in6>(),
        sin6_family: family(libc::AF_INET6),
        sin6_port: port.to_be(),
        sin6_flowinfo: 0,
        sin6_addr: libc::in6_addr {
            s6_addr: ip.octets(),
        },
        sin6_scope_id: 0,
    };
    (storage, socklen(mem::size_of::<libc::sockaddr_in6>()))
}

/// Convert the system's form back, collapsing a v4-mapped address to IPv4.
///
/// **Structural, never textual.** A v4-mapped address contains colons in its
/// text form and is IPv4; deciding by searching for one removes every v4
/// candidate and kills connectivity on v4-only paths.
pub(super) fn from_storage(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
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
    use std::net::UdpSocket;

    /// A receiver on loopback in one family that reports each datagram's
    /// type of service, as a peer of that family sees it.
    fn marked_receiver(v6: bool) -> (UdpSocket, SocketAddr) {
        let socket = if v6 {
            UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).expect("v6 receiver")
        } else {
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("v4 receiver")
        };
        let (level, name) = if v6 {
            (libc::IPPROTO_IPV6, libc::IPV6_RECVTCLASS)
        } else {
            (libc::IPPROTO_IP, libc::IP_RECVTOS)
        };
        let on: libc::c_int = 1;
        // SAFETY: an int option and its exact size, on a socket we own.
        let rc = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                level,
                name,
                (&raw const on).cast(),
                socklen(mem::size_of::<libc::c_int>()),
            )
        };
        assert_eq!(rc, 0, "the receiver cannot report the mark");
        let addr = socket.local_addr().expect("addr");
        (socket, addr)
    }

    /// The mark the next datagram on `receiver` arrived with.
    fn arrived_mark(receiver: &UdpSocket) -> u8 {
        let mut ready = libc::pollfd {
            fd: receiver.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd and its count.
        assert_eq!(
            unsafe { libc::poll(&raw mut ready, 1, 1000) },
            1,
            "nothing arrived"
        );
        let mut bytes = [0u8; 64];
        #[repr(align(8))]
        struct Control([u8; 64]);
        let mut control = Control([0u8; 64]);
        let mut iov = libc::iovec {
            iov_base: bytes.as_mut_ptr().cast(),
            iov_len: bytes.len(),
        };
        // SAFETY: msghdr is plain data; zeroing it is the documented way to start.
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &raw mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = socklen(control.0.len());
        // SAFETY: every pointer in `msg` names storage live for the call.
        assert!(unsafe { libc::recvmsg(receiver.as_raw_fd(), &raw mut msg, 0) } > 0);
        // SAFETY: the control storage and length the system wrote; the macros
        // stay inside it and the reads are unaligned-tolerant.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&raw const msg);
            while !cmsg.is_null() {
                let (level, kind) = ((*cmsg).cmsg_level, (*cmsg).cmsg_type);
                if level == libc::IPPROTO_IP && kind == libc::IP_RECVTOS {
                    return core::ptr::read_unaligned(libc::CMSG_DATA(cmsg));
                }
                if level == libc::IPPROTO_IPV6 && kind == libc::IPV6_TCLASS {
                    let class =
                        core::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>());
                    return u8::try_from(class).expect("a class is one byte");
                }
                cmsg = libc::CMSG_NXTHDR(&raw const msg, cmsg);
            }
        }
        panic!("no mark was reported");
    }

    /// **Both families leave marked, pinned or not.** The socket's v6 class
    /// reaches v6 sends alone and its v4 type of service is refused, so a v4
    /// send without its own control message arrives at zero.
    #[test]
    fn sends_leave_marked_in_both_families() {
        let sender = Socket::open(0).expect("sender");
        let ef = u8::try_from(DSCP_EF).expect("a one-byte mark");
        for v6 in [false, true] {
            let (receiver, to) = marked_receiver(v6);
            sender.send_to(b"free", to).expect("send");
            assert_eq!(
                arrived_mark(&receiver),
                ef,
                "an unpinned send left unmarked, v6={v6}"
            );
            super::super::send::pinned_send(&sender, b"pinned", to, to.ip()).expect("send");
            assert_eq!(
                arrived_mark(&receiver),
                ef,
                "a pinned send left unmarked, v6={v6}"
            );
        }
    }

    /// A v4 datagram past the loopback's largest is refused rather than
    /// fragmented, so a path probe cannot read pieces as success: the v6
    /// option covers v4 sends, and without it they leave in pieces.
    #[test]
    fn an_oversized_v4_send_is_refused() {
        let sender = Socket::open(0).expect("sender");
        let receiver = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("receiver");
        let to = receiver.local_addr().expect("addr");
        let error = sender
            .send_to(&[0u8; 20_000], to)
            .expect_err("the datagram was fragmented rather than refused");
        assert_eq!(error.raw_os_error(), Some(libc::EMSGSIZE));
    }

    /// **A port the system chooses is never one another socket holds for
    /// IPv4.** Its own choice for a dual-stack socket ignores such ports, so
    /// with a hundred of the 16384 ephemeral ports held, a thousand opens
    /// without the guard land on one about six times, and on none in about
    /// one run in five hundred. A hundred held stays clear of the descriptor
    /// limit with the rest of the suite running beside it.
    #[test]
    fn a_chosen_port_is_never_held_for_ipv4() {
        let held: Vec<UdpSocket> = (0..100)
            .map(|_| UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("hold"))
            .collect();
        let ports: std::collections::HashSet<u16> = held
            .iter()
            .map(|socket| socket.local_addr().expect("addr").port())
            .collect();
        for _ in 0..1000 {
            let port = Socket::open(0)
                .expect("open")
                .local_addr()
                .expect("addr")
                .port();
            assert!(
                !ports.contains(&port),
                "took {port}, which is held for IPv4"
            );
        }
    }
}
