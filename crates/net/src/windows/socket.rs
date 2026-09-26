//! The media socket through Winsock: the handle, the option set, and the
//! address conversions.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::io;
use std::mem;
use std::os::windows::io::{AsRawSocket, FromRawSocket, OwnedSocket};
use std::sync::OnceLock;

use windows_sys::Win32::Networking::WinSock as ws;

use crate::socket::{DEFAULT_TTL, WANT_RCVBUF, WANT_SNDBUF};

/// The system's last socket error, as an I/O error.
pub(super) fn last_error() -> io::Error {
    // SAFETY: reads the calling thread's last error and nothing else.
    io::Error::from_raw_os_error(unsafe { ws::WSAGetLastError() })
}

/// Sizes into the system's length type. Every value passed is a struct size,
/// far below the type's range, and the saturating fallback keeps the
/// conversion total rather than panicking on a case that cannot arise.
pub(super) fn len_of<T>() -> i32 {
    i32::try_from(mem::size_of::<T>()).unwrap_or(i32::MAX)
}

/// Start the socket library once per process, before the first socket.
///
/// The reference it takes is never given back: a library inside someone
/// else's process cannot know when its last socket has closed, and the system
/// releases it with the process.
fn start() -> io::Result<()> {
    static STARTED: OnceLock<i32> = OnceLock::new();
    let code = *STARTED.get_or_init(|| {
        // SAFETY: WSADATA is plain data the call fills in; zero is a valid
        // starting value for every field.
        let mut data: ws::WSADATA = unsafe { mem::zeroed() };
        // SAFETY: version 2.2 and writable storage of the right type.
        unsafe { ws::WSAStartup(0x0202, &raw mut data) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

/// A bound UDP socket with the full option set applied.
#[derive(Debug)]
pub struct Socket {
    socket: OwnedSocket,
    granted_rcvbuf: i32,
    granted_sndbuf: i32,
}

impl Socket {
    /// One attempt: a fresh socket, the full option set, and a bind.
    pub(crate) fn bound(port: u16) -> io::Result<Self> {
        start()?;
        // SAFETY: socket creation with constant arguments. Overlapped, so the
        // receive can be posted to the completion port; not inherited by a
        // child the application starts.
        let raw = unsafe {
            ws::WSASocketW(
                i32::from(ws::AF_INET6),
                ws::SOCK_DGRAM,
                ws::IPPROTO_UDP,
                core::ptr::null(),
                0,
                ws::WSA_FLAG_OVERLAPPED | ws::WSA_FLAG_NO_HANDLE_INHERIT,
            )
        };
        if raw == ws::INVALID_SOCKET {
            return Err(last_error());
        }
        let socket = Self {
            // SAFETY: a fresh socket we own and have registered nowhere else;
            // the owned handle closes it on drop.
            socket: unsafe { OwnedSocket::from_raw_socket(raw as u64) },
            granted_rcvbuf: 0,
            granted_sndbuf: 0,
        };
        socket.configure()?;
        socket.bind(port)?;

        let granted_rcvbuf = socket.get_int(ws::SOL_SOCKET, ws::SO_RCVBUF)?;
        let granted_sndbuf = socket.get_int(ws::SOL_SOCKET, ws::SO_SNDBUF)?;
        Ok(Self {
            granted_rcvbuf,
            granted_sndbuf,
            ..socket
        })
    }

    fn configure(&self) -> io::Result<()> {
        // Dual stack, and first: a fresh socket here serves one family only,
        // and every v4-level option below is refused until this is cleared.
        self.set_int(ws::IPPROTO_IPV6, ws::IPV6_V6ONLY, 0)?;

        // Ask high and accept what the system grants; the granted value is
        // logged at open, because a silently clamped request is invisible
        // until a burst is lost.
        self.set_int(ws::SOL_SOCKET, ws::SO_RCVBUF, WANT_RCVBUF)?;
        self.set_int(ws::SOL_SOCKET, ws::SO_SNDBUF, WANT_SNDBUF)?;

        // The address each datagram arrived at, on both families.
        self.set_int(ws::IPPROTO_IPV6, ws::IPV6_PKTINFO, 1)?;
        self.set_int(ws::IPPROTO_IP, ws::IP_PKTINFO, 1)?;

        // No traffic class here. A per-socket type of service is accepted and
        // then ignored -- it reaches the wire as zero -- and the v6 traffic
        // class is refused outright, so the established path is marked per
        // destination instead (`Io::mark`).

        // Do not fragment, on both families, so an oversized probe fails fast
        // rather than being split and arriving anyway. The discovery pair and
        // not the don't-fragment options: those are refused on a dual-stack
        // socket.
        self.set_int(ws::IPPROTO_IP, ws::IP_MTU_DISCOVER, ws::IP_PMTUDISC_DO)?;
        self.set_int(ws::IPPROTO_IPV6, ws::IPV6_MTU_DISCOVER, ws::IP_PMTUDISC_DO)?;

        self.set_ttl(DEFAULT_TTL)?;

        // An unreachable answering an earlier send, and a hop limit expiring
        // on a mapping probe, each fail the next receive on this socket
        // unless reported nowhere. The probe provokes the second by design.
        self.ioctl_off(ws::SIO_UDP_CONNRESET)?;
        self.ioctl_off(ws::SIO_UDP_NETRESET)?;

        self.set_nonblocking()
    }

    fn bind(&self, port: u16) -> io::Result<()> {
        let addr = ws::SOCKADDR_IN6 {
            sin6_family: ws::AF_INET6,
            sin6_port: port.to_be(),
            ..Default::default()
        };
        // SAFETY: a fully initialised address and its exact size.
        let rc = unsafe {
            ws::bind(
                self.raw(),
                (&raw const addr).cast(),
                len_of::<ws::SOCKADDR_IN6>(),
            )
        };
        if rc != 0 {
            return Err(last_error());
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
        let mut storage = ws::SOCKADDR_STORAGE::default();
        let mut len = len_of::<ws::SOCKADDR_STORAGE>();
        // SAFETY: the storage is large enough for any family and `len`
        // describes it exactly; the system writes at most that many bytes.
        let rc = unsafe { ws::getsockname(self.raw(), (&raw mut storage).cast(), &raw mut len) };
        if rc != 0 {
            return Err(last_error());
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
        self.set_int(ws::IPPROTO_IP, ws::IP_TTL, i32::from(ttl))?;
        self.set_int(ws::IPPROTO_IPV6, ws::IPV6_UNICAST_HOPS, i32::from(ttl))
    }

    /// The current hop limit, read back from the system.
    pub fn ttl(&self) -> io::Result<u8> {
        let value = self.get_int(ws::IPPROTO_IP, ws::IP_TTL)?;
        u8::try_from(value).map_err(|_| io::Error::other("ttl outside the byte range"))
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        let mut on: u32 = 1;
        // SAFETY: the command takes a pointer to one u32, which is passed.
        let rc = unsafe { ws::ioctlsocket(self.raw(), ws::FIONBIO, &raw mut on) };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Switch off one of the socket's boolean controls.
    fn ioctl_off(&self, code: u32) -> io::Result<()> {
        let off: i32 = 0;
        let mut returned: u32 = 0;
        // SAFETY: the control takes one BOOL in and nothing out; the input
        // pointer and length describe `off` exactly, and the call is
        // synchronous (no overlapped structure).
        let rc = unsafe {
            ws::WSAIoctl(
                self.raw(),
                code,
                (&raw const off).cast(),
                4,
                core::ptr::null_mut(),
                0,
                &raw mut returned,
                core::ptr::null_mut(),
                None,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn set_int(&self, level: i32, name: i32, value: i32) -> io::Result<()> {
        // SAFETY: the option value is an int and the length passed is its
        // exact size, which is what every option used here expects.
        let rc = unsafe {
            ws::setsockopt(
                self.raw(),
                level,
                name,
                (&raw const value).cast(),
                len_of::<i32>(),
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    fn get_int(&self, level: i32, name: i32) -> io::Result<i32> {
        let mut value: i32 = 0;
        let mut len = len_of::<i32>();
        // SAFETY: `value` is an int and `len` describes it exactly; the
        // system writes at most that many bytes and updates `len`.
        let rc = unsafe {
            ws::getsockopt(
                self.raw(),
                level,
                name,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(value)
    }

    /// Send one datagram.
    pub fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<usize> {
        let (addr, addr_len) = to_storage(to);
        let len =
            i32::try_from(datagram.len()).map_err(|_| io::Error::other("datagram too large"))?;
        // SAFETY: `datagram` is a valid slice of `len` bytes and `addr` a
        // fully initialised address of exactly `addr_len` bytes.
        let sent = unsafe {
            ws::sendto(
                self.raw(),
                datagram.as_ptr(),
                len,
                0,
                (&raw const addr).cast(),
                addr_len,
            )
        };
        if sent < 0 {
            return Err(last_error());
        }
        Ok(usize::try_from(sent).unwrap_or(0))
    }

    /// Connect to `to`, which a UDP socket takes as a filter on what it
    /// receives and a default for where it sends.
    pub(super) fn connect(&self, to: SocketAddr) -> io::Result<()> {
        let (addr, len) = to_storage(to);
        // SAFETY: a fully initialised address and its exact size.
        let rc = unsafe { ws::connect(self.raw(), (&raw const addr).cast(), len) };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// Undo [`Socket::connect`]: an all-zero address disconnects a UDP
    /// socket, and its local address returns to the wildcard it was bound to.
    pub(super) fn disconnect(&self) -> io::Result<()> {
        let zero = ws::SOCKADDR_IN6 {
            sin6_family: ws::AF_INET6,
            ..Default::default()
        };
        // SAFETY: a fully initialised address and its exact size.
        let rc = unsafe {
            ws::connect(
                self.raw(),
                (&raw const zero).cast(),
                len_of::<ws::SOCKADDR_IN6>(),
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    /// The handle, for the calls in this module's siblings.
    pub(super) fn raw(&self) -> ws::SOCKET {
        // A socket handle is pointer sized on every Windows target, so the
        // conversion from the standard library's 64-bit form is exact.
        usize::try_from(self.socket.as_raw_socket()).unwrap_or(ws::INVALID_SOCKET)
    }
}

/// Convert a socket address into the system's form.
pub(super) fn to_storage(addr: SocketAddr) -> (ws::SOCKADDR_IN6, i32) {
    // The socket is dual stack, so a v4 destination goes out as v4-mapped.
    let (ip, port) = match addr {
        SocketAddr::V4(v4) => (v4.ip().to_ipv6_mapped(), v4.port()),
        SocketAddr::V6(v6) => (*v6.ip(), v6.port()),
    };
    let storage = ws::SOCKADDR_IN6 {
        sin6_family: ws::AF_INET6,
        sin6_port: port.to_be(),
        sin6_addr: ws::IN6_ADDR {
            u: ws::IN6_ADDR_0 { Byte: ip.octets() },
        },
        ..Default::default()
    };
    (storage, len_of::<ws::SOCKADDR_IN6>())
}

/// Convert the system's form back, collapsing a v4-mapped address to IPv4.
///
/// **Structural, never textual.** A v4-mapped address contains colons in its
/// text form and is IPv4; deciding by searching for one removes every v4
/// candidate and kills connectivity on v4-only paths.
pub(super) fn from_storage(storage: &ws::SOCKADDR_STORAGE) -> Option<SocketAddr> {
    match storage.ss_family {
        ws::AF_INET6 => {
            // SAFETY: the family says this is a SOCKADDR_IN6, and the storage
            // is defined to be large enough and aligned for it.
            let v6 = unsafe { &*core::ptr::from_ref(storage).cast::<ws::SOCKADDR_IN6>() };
            // SAFETY: every view of the address union is plain bytes.
            let ip = Ipv6Addr::from(unsafe { v6.sin6_addr.u.Byte });
            let port = u16::from_be(v6.sin6_port);
            Some(match ip.to_ipv4_mapped() {
                Some(v4) => SocketAddr::V4(SocketAddrV4::new(v4, port)),
                None => SocketAddr::V6(SocketAddrV6::new(
                    ip,
                    port,
                    v6.sin6_flowinfo,
                    // SAFETY: as above, the scope union is a plain word.
                    unsafe { v6.Anonymous.sin6_scope_id },
                )),
            })
        }
        ws::AF_INET => {
            // SAFETY: as above, for the v4 layout.
            let v4 = unsafe { &*core::ptr::from_ref(storage).cast::<ws::SOCKADDR_IN>() };
            // SAFETY: every view of the address union is plain bytes; the
            // word is in network order, so its bytes are the octets.
            let word = unsafe { v4.sin_addr.S_un.S_addr };
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::from(word.to_ne_bytes())),
                u16::from_be(v4.sin_port),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Neither family fragments.** A socket left at the v6 default fragments
    /// an oversized datagram locally instead of refusing it, and the path
    /// probe reads an arrival as the size having worked.
    #[test]
    fn neither_family_fragments_an_oversized_datagram() {
        let socket = Socket::open(0).expect("open");

        assert_eq!(
            socket
                .get_int(ws::IPPROTO_IP, ws::IP_MTU_DISCOVER)
                .expect("v4 discovery"),
            ws::IP_PMTUDISC_DO,
            "the v4 path would fragment rather than refuse"
        );
        assert_eq!(
            socket
                .get_int(ws::IPPROTO_IPV6, ws::IPV6_MTU_DISCOVER)
                .expect("v6 discovery"),
            ws::IP_PMTUDISC_DO,
            "the v6 path would fragment rather than refuse"
        );
    }

    /// Dual stack is what lets the v4-level options apply at all, so it is
    /// read back rather than trusted.
    #[test]
    fn the_socket_serves_both_families() {
        let socket = Socket::open(0).expect("open");
        assert_eq!(
            socket
                .get_int(ws::IPPROTO_IPV6, ws::IPV6_V6ONLY)
                .expect("v6 only"),
            0,
            "the socket serves one family"
        );
    }

    /// A v4-mapped address is IPv4 and must come back as such, whatever the
    /// dual-stack socket hands us.
    #[test]
    fn a_v4_mapped_source_is_reported_as_v4() {
        let mapped = Ipv4Addr::new(198, 51, 100, 7).to_ipv6_mapped();
        let (storage, _) = to_storage(SocketAddr::new(IpAddr::V6(mapped), 9000));

        let mut generic = ws::SOCKADDR_STORAGE::default();
        // SAFETY: the storage is the larger type and both are plain data;
        // this is the reinterpretation the system interface itself makes.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (&raw const storage).cast::<u8>(),
                (&raw mut generic).cast::<u8>(),
                mem::size_of::<ws::SOCKADDR_IN6>(),
            );
        }

        assert_eq!(
            from_storage(&generic),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)),
                9000
            ))
        );
    }
}
