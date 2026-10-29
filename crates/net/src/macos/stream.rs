//! The relay's stream through the system's own interface: a non-blocking TCP
//! socket, connected in the background and watched on the wake's kernel queue
//! (docs/02-io-shell.md 6).
//!
//! **Opened in the relay's own family**, not dual stack: a dual-stack socket
//! refuses the v4 type of service, and a stream has no per-send control
//! message to carry it instead, so a v4 relay's stream would leave unmarked.
//!
//! **The socket never raises a broken-pipe signal** (`SO_NOSIGPIPE`). A send
//! on a stream the relay has closed would otherwise raise one whose
//! disposition is the host process's, not ours, and its default ends the
//! process; the system has no per-send flag for it.

use core::net::{IpAddr, SocketAddr};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::queue;
use super::socket::{set_flags, socklen};
use super::wake::Wake;
use crate::shell::Ready;
use crate::socket::DSCP_EF;
use crate::stream::Written;

/// The stream's socket and the wake it shares the loop's queue with.
#[derive(Debug)]
pub(crate) struct StreamIo {
    fd: OwnedFd,
    wake: Wake,
    connecting: bool,
    /// Whether the queue reports room to write. Only while connecting or a
    /// write is pending: a writable stream is reported on every wait, which
    /// would be a spin.
    watching: bool,
}

impl StreamIo {
    /// A TCP socket in the server's family, its connect to `server` begun.
    pub(crate) fn connect(server: SocketAddr, wake: Wake) -> io::Result<Self> {
        // Structural: a v4-mapped server is a v4 one.
        let server = match server {
            SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
                Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
                None => server,
            },
            SocketAddr::V4(_) => server,
        };
        let family = if server.is_ipv4() {
            libc::AF_INET
        } else {
            libc::AF_INET6
        };
        // SAFETY: socket(2) with constant arguments; the descriptor is handed
        // straight to OwnedFd, which closes it on drop.
        let raw = unsafe { libc::socket(family, libc::SOCK_STREAM, libc::IPPROTO_TCP) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut io = Self {
            // SAFETY: a fresh descriptor we own and have registered nowhere.
            fd: unsafe { OwnedFd::from_raw_fd(raw) },
            wake,
            connecting: true,
            watching: false,
        };
        set_flags(raw)?;
        io.set_int(libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1)?;
        // A pass's messages are written together already; nothing waits to
        // gather more.
        io.set_int(libc::IPPROTO_TCP, libc::TCP_NODELAY, 1)?;
        // Marked at open, as the datagram socket is. No buffer sizes: the
        // stack tunes a stream's own.
        if server.is_ipv4() {
            io.set_int(libc::IPPROTO_IP, libc::IP_TOS, DSCP_EF)?;
        } else {
            io.set_int(libc::IPPROTO_IPV6, libc::IPV6_TCLASS, DSCP_EF)?;
        }

        let (addr, len) = native(server);
        // SAFETY: an address structure and its exact length.
        let rc = unsafe { libc::connect(raw, (&raw const addr).cast(), len) };
        if rc == 0 {
            io.connecting = false;
        } else {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(error);
            }
        }

        // Joined once the connect is under way, watched for writing until it
        // ends; one already over is still reported, the filter being level.
        let ident = usize::try_from(raw).unwrap_or(usize::MAX);
        queue::apply(
            io.wake.queue(),
            &[
                queue::event(ident, libc::EVFILT_READ, libc::EV_ADD, 0, 0),
                queue::event(ident, libc::EVFILT_WRITE, libc::EV_ADD, 0, 0),
            ],
        )?;
        io.watching = true;
        Ok(io)
    }

    pub(crate) fn wake(&self) -> &Wake {
        &self.wake
    }

    pub(crate) fn take_wake(&mut self) -> io::Result<bool> {
        self.wake.take()
    }

    pub(crate) fn connected(&self) -> bool {
        !self.connecting
    }

    /// Never: a send here is taken at once, in part or not at all.
    pub(crate) fn sending(&self) -> bool {
        false
    }

    /// Never: nothing is held for the wait to finish.
    pub(crate) fn finished(&mut self) -> Option<io::Result<usize>> {
        None
    }

    /// The flow is marked at open; nothing to do per destination.
    pub(crate) fn mark(&mut self, _to: SocketAddr) {}

    /// Wait for the stream -- bytes, the connect's end, room to write while
    /// `writing` -- or the wake, or the deadline; at once when `now`.
    pub(crate) fn wait(&mut self, timeout_ms: f64, writing: bool, now: bool) -> io::Result<Ready> {
        let ident = usize::try_from(self.fd.as_raw_fd()).unwrap_or(usize::MAX);
        let watch = self.connecting || writing;
        let mut changes = [queue::NONE; 2];
        let mut count = 0;
        if watch != self.watching {
            let flags = if watch {
                libc::EV_ENABLE
            } else {
                libc::EV_DISABLE
            };
            if let Some(change) = changes.get_mut(count) {
                *change = queue::event(ident, libc::EVFILT_WRITE, flags, 0, 0);
                count += 1;
            }
        }
        if !now {
            if let Some(change) = changes.get_mut(count) {
                *change = queue::deadline(timeout_ms);
                count += 1;
            }
        }
        let mut events = [queue::NONE; queue::EVENTS];
        let got = queue::wait(
            self.wake.queue(),
            changes.get(..count).unwrap_or_default(),
            &mut events,
            now,
        )?;
        self.watching = watch;

        let (mut stream, mut wake) = (false, false);
        for event in events.get(..got).unwrap_or_default() {
            // The structure is packed: the field is read by value.
            let filter = event.filter;
            if filter == libc::EVFILT_READ || filter == libc::EVFILT_WRITE {
                stream = true;
            } else if filter == libc::EVFILT_USER {
                wake = true;
            }
        }
        if self.connecting && stream {
            // Connected, or refused: the socket's pending error says which.
            let error = self.get_int(libc::SOL_SOCKET, libc::SO_ERROR)?;
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            self.connecting = false;
        }
        Ok(Ready {
            socket: stream || now,
            wake,
        })
    }

    /// Read what has arrived into `into`: zero is the stream's end, and
    /// `WouldBlock` that nothing is there.
    pub(crate) fn read(&mut self, into: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: a writable buffer and its exact length.
            let rc = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    into.as_mut_ptr().cast(),
                    into.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if let Ok(read) = usize::try_from(rc) {
                return Ok(read);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Write as much of `from` as the socket takes now.
    pub(crate) fn write(&mut self, from: &[u8]) -> io::Result<Written> {
        loop {
            // SAFETY: a readable buffer and its exact length.
            let rc = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    from.as_ptr().cast(),
                    from.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if let Ok(taken) = usize::try_from(rc) {
                return Ok(Written::Now(taken));
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(Written::Now(0)),
                _ => return Err(error),
            }
        }
    }

    fn set_int(&self, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
        // SAFETY: an int option and its exact size.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.as_raw_fd(),
                level,
                name,
                (&raw const value).cast(),
                socklen(mem::size_of::<libc::c_int>()),
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn get_int(&self, level: libc::c_int, name: libc::c_int) -> io::Result<libc::c_int> {
        let mut value: libc::c_int = 0;
        let mut len = socklen(mem::size_of::<libc::c_int>());
        // SAFETY: an int option buffer and its length, both writable.
        let rc = unsafe {
            libc::getsockopt(
                self.fd.as_raw_fd(),
                level,
                name,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        if rc == 0 {
            Ok(value)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

/// The server's address in its own family's structure, and that structure's
/// length.
fn native(server: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: sockaddr_storage is plain data; all zeroes is valid, and it is
    // large enough and aligned for either family's structure written into it.
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let len = match server {
        SocketAddr::V4(v4) => {
            let addr = libc::sockaddr_in {
                sin_len: u8::try_from(mem::size_of::<libc::sockaddr_in>()).unwrap_or(u8::MAX),
                sin_family: libc::sa_family_t::try_from(libc::AF_INET).unwrap_or(0),
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*v4.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: the storage is larger than and aligned for a sockaddr_in.
            unsafe { core::ptr::write((&raw mut storage).cast::<libc::sockaddr_in>(), addr) };
            mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(v6) => {
            let addr = libc::sockaddr_in6 {
                sin6_len: u8::try_from(mem::size_of::<libc::sockaddr_in6>()).unwrap_or(u8::MAX),
                sin6_family: libc::sa_family_t::try_from(libc::AF_INET6).unwrap_or(0),
                sin6_port: v6.port().to_be(),
                sin6_flowinfo: v6.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: v6.ip().octets(),
                },
                sin6_scope_id: v6.scope_id(),
            };
            // SAFETY: the storage is larger than and aligned for a sockaddr_in6.
            unsafe { core::ptr::write((&raw mut storage).cast::<libc::sockaddr_in6>(), addr) };
            mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (storage, socklen(len))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, Ipv6Addr};
    use std::net::TcpListener;

    /// **A stream to a v4 relay leaves marked**, which a dual-stack socket
    /// cannot do: it refuses the v4 type of service. Read back from the
    /// socket, in both families, once connected.
    #[test]
    fn the_stream_is_marked_in_its_relay_s_family() {
        for (ip, level, name) in [
            (
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                libc::IPPROTO_IP,
                libc::IP_TOS,
            ),
            (
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                libc::IPPROTO_IPV6,
                libc::IPV6_TCLASS,
            ),
        ] {
            let listener = TcpListener::bind(SocketAddr::new(ip, 0)).expect("listen");
            let server = listener.local_addr().expect("addr");
            let mut io = StreamIo::connect(server, Wake::new().expect("wake")).expect("connect");
            let (_peer, _) = listener.accept().expect("accept");
            while !io.connected() {
                io.wait(1000.0, false, false).expect("wait");
            }
            assert_eq!(io.get_int(level, name).expect("mark"), DSCP_EF, "{ip}");
            assert_eq!(
                io.get_int(libc::SOL_SOCKET, libc::SO_NOSIGPIPE)
                    .expect("signal"),
                1,
                "a write after a reset would raise a signal, {ip}"
            );
        }
    }
}
