//! The default gateway, the interface a search leaves by, a connection a stop
//! can cut short, and the machine's name and identifier, on Linux.

#![allow(unsafe_code)]

use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;
use std::io;
use std::net::{TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// The default route's gateway, from the routing table. Read as bytes: an
/// interface's name need not be text, and one such line must not hide the
/// rest.
pub(crate) fn gateway() -> Option<Ipv4Addr> {
    let table = std::fs::read("/proc/net/route").ok()?;
    crate::route::default_gateway(&String::from_utf8_lossy(&table))
}

/// Send `socket`'s multicast out of the interface that holds `local`.
pub(crate) fn multicast_from(socket: &UdpSocket, local: Ipv4Addr) -> io::Result<()> {
    let address = libc::in_addr {
        s_addr: u32::from_ne_bytes(local.octets()),
    };
    let len =
        libc::socklen_t::try_from(core::mem::size_of_val(&address)).map_err(io::Error::other)?;
    // SAFETY: a live socket's descriptor, and an option whose value is the
    // address it names, read for its size during the call only.
    let rc = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&raw const address).cast(),
            len,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A TCP connection being made, waited for in pieces so that a stop is seen
/// between them: the system's own connect waits whole.
#[derive(Debug)]
pub(crate) struct Connecting {
    fd: OwnedFd,
}

impl Connecting {
    /// Begin connecting to `to`. Nothing waits here.
    pub(crate) fn start(to: SocketAddrV4) -> io::Result<Self> {
        let flags = libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC;
        // SAFETY: plain arguments; the descriptor returned is owned below.
        let raw = unsafe { libc::socket(libc::AF_INET, flags, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor this call opened and nothing else holds.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let address = libc::sockaddr_in {
            sin_family: libc::sa_family_t::try_from(libc::AF_INET).map_err(io::Error::other)?,
            sin_port: to.port().to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(to.ip().octets()),
            },
            sin_zero: [0; 8],
        };
        let len = libc::socklen_t::try_from(core::mem::size_of_val(&address))
            .map_err(io::Error::other)?;
        // SAFETY: a live descriptor, and an address of `len` bytes read during
        // the call only.
        let rc = unsafe { libc::connect(fd.as_raw_fd(), (&raw const address).cast(), len) };
        if rc != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error);
            }
        }
        Ok(Self { fd })
    }

    /// Wait up to `wait` for the connection: the stream once it is made, a
    /// blocking one as the caller's reads expect, or this again while it is
    /// still being made.
    pub(crate) fn wait(self, wait: Duration) -> io::Result<Result<TcpStream, Self>> {
        let mut ready = libc::pollfd {
            fd: self.fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ms = libc::c_int::try_from(wait.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
        // SAFETY: one record, live for the call.
        let rc = unsafe { libc::poll(&raw mut ready, 1, ms) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::Interrupted {
                Ok(Err(self))
            } else {
                Err(error)
            };
        }
        if rc == 0 {
            return Ok(Err(self));
        }
        // Writable or failed: the connect's own outcome says which.
        let mut outcome: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(core::mem::size_of_val(&outcome))
            .map_err(io::Error::other)?;
        // SAFETY: a live descriptor, and an integer and its length written
        // during the call only.
        let rc = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&raw mut outcome).cast(),
                &raw mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        if outcome != 0 {
            return Err(io::Error::from_raw_os_error(outcome));
        }
        let stream = TcpStream::from(self.fd);
        stream.set_nonblocking(false)?;
        Ok(Ok(stream))
    }
}

/// The machine's host name.
pub(crate) fn machine_name() -> Option<String> {
    // Longer than any host name the kernel holds, terminator included.
    let mut name = [0u8; 256];
    // SAFETY: a buffer of its own length, written by the call and live for it.
    let rc = unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len()) };
    if rc != 0 {
        return None;
    }
    let end = name.iter().position(|&byte| byte == 0)?;
    core::str::from_utf8(name.get(..end)?)
        .ok()
        .map(str::to_owned)
}

/// The installation's identifier, as the system keeps it. It must not leave
/// the machine as it is: the caller reduces it with a keyed hash first.
pub(crate) fn machine_id() -> Option<Vec<u8>> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .find_map(|path| {
            let id = std::fs::read(path).ok()?;
            let id = id.trim_ascii();
            (!id.is_empty()).then(|| id.to_vec())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine with a route beyond its network names its gateway, neither
    /// loopback nor nothing.
    #[test]
    fn the_default_route_names_its_gateway() {
        let beyond = SocketAddrV4::new(Ipv4Addr::new(192, 0, 2, 1), 9);
        if crate::mapper::local_toward(beyond).is_none() {
            return;
        }
        let gateway = gateway().expect("a default route with no gateway found");
        assert!(
            !gateway.is_loopback() && !gateway.is_unspecified(),
            "{gateway}"
        );
    }
}
