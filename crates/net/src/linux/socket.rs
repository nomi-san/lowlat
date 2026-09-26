//! The media socket through the kernel's own interface: the descriptor, the
//! option set, and the address conversions.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use crate::socket::{DEFAULT_TTL, DSCP_EF, WANT_RCVBUF, WANT_SNDBUF};

/// Sizes into the kernel's length type.
///
/// Every value passed is a compile-time struct size, orders of magnitude below
/// the type's range. The saturating fallback keeps the conversion total rather
/// than panicking on a case that cannot arise.
pub(super) fn socklen(bytes: usize) -> libc::socklen_t {
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
    /// One attempt: a fresh descriptor, the full option set, and a bind.
    pub(crate) fn bound(port: u16) -> io::Result<Self> {
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
        //
        // **Both families, because neither setting carries to the other.** A
        // v6 socket left at its default fragments locally rather than refusing,
        // and the path probe reads that as the size having worked. IPv6's
        // minimum is 1280 and the ladder climbs past it, so the rungs above
        // that would each be reported reachable on a path that can only carry
        // them in pieces.
        self.set_int(
            libc::IPPROTO_IP,
            libc::IP_MTU_DISCOVER,
            libc::IP_PMTUDISC_DO,
        )?;
        self.set_int(
            libc::IPPROTO_IPV6,
            libc::IPV6_MTU_DISCOVER,
            libc::IPV6_PMTUDISC_DO,
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

    /// The descriptor, for the calls in this module's siblings.
    pub(super) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Convert a socket address into the kernel's form.
pub(super) fn to_storage(addr: SocketAddr) -> (libc::sockaddr_in6, libc::socklen_t) {
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

    /// **Neither family fragments, and neither setting carries to the other.**
    ///
    /// A socket left at the v6 default fragments an oversized datagram locally
    /// instead of refusing it, and the path probe reads an arrival as the size
    /// having worked. Both halves are asserted here so that setting one and
    /// calling the option done fails on this test rather than on a path whose
    /// minimum is 1280 and whose ladder climbs past it.
    #[test]
    fn neither_family_fragments_an_oversized_datagram() {
        let socket = Socket::open(0).expect("open");

        assert_eq!(
            socket
                .get_int(libc::IPPROTO_IP, libc::IP_MTU_DISCOVER)
                .expect("v4 discovery"),
            libc::IP_PMTUDISC_DO,
            "the v4 path would fragment rather than refuse"
        );
        assert_eq!(
            socket
                .get_int(libc::IPPROTO_IPV6, libc::IPV6_MTU_DISCOVER)
                .expect("v6 discovery"),
            libc::IPV6_PMTUDISC_DO,
            "the v6 path would fragment rather than refuse"
        );
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
