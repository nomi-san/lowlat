//! The default gateway, the interface a search leaves by, a connection a stop
//! can cut short, and the machine's name and identifier, on macOS.

#![allow(unsafe_code)]

use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;
use std::io;
use std::net::{TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// Beyond the local network, so the routing table answers with the route it
/// gives such a destination. Nothing is sent to it.
const BEYOND: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 1);

/// The routing messages read while looking for the answer to ours: the
/// socket hears every routing message on the system.
const READS: usize = 16;

/// A routing socket's answer arrives with the request; this bounds a wait
/// for one that never comes.
const ANSWER: Duration = Duration::from_millis(500);

/// The request: the header, then the destination it asks about.
#[repr(C)]
struct Request {
    header: libc::rt_msghdr,
    destination: libc::sockaddr_in,
}

/// An IPv4 socket address in the system's form, which carries its own length.
fn sockaddr(address: SocketAddrV4) -> io::Result<libc::sockaddr_in> {
    Ok(libc::sockaddr_in {
        sin_len: u8::try_from(core::mem::size_of::<libc::sockaddr_in>())
            .map_err(io::Error::other)?,
        sin_family: libc::sa_family_t::try_from(libc::AF_INET).map_err(io::Error::other)?,
        sin_port: address.port().to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(address.ip().octets()),
        },
        sin_zero: [0; 8],
    })
}

/// The gateway of the route the system takes beyond the local network, asked
/// of the routing table for one such destination: the route it would use,
/// not every default it lists -- a tunnel can list a default of its own,
/// scoped to itself, that ordinary traffic does not take.
pub(crate) fn gateway() -> Option<Ipv4Addr> {
    // SAFETY: plain arguments; the descriptor returned is owned below.
    let raw = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW, libc::AF_INET) };
    if raw < 0 {
        return None;
    }
    // SAFETY: a descriptor this call opened and nothing else holds.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    let seq = 1;
    // SAFETY: plain data; all zeroes is a valid value.
    let mut request: Request = unsafe { core::mem::zeroed() };
    request.header.rtm_msglen = u16::try_from(core::mem::size_of::<Request>()).ok()?;
    request.header.rtm_version = u8::try_from(libc::RTM_VERSION).ok()?;
    request.header.rtm_type = u8::try_from(libc::RTM_GET).ok()?;
    request.header.rtm_addrs = libc::RTA_DST;
    request.header.rtm_seq = seq;
    request.destination = sockaddr(SocketAddrV4::new(BEYOND, 0)).ok()?;
    // SAFETY: the request's own bytes, read during the call only. Asking
    // changes no route.
    let wrote = unsafe {
        libc::write(
            fd.as_raw_fd(),
            (&raw const request).cast(),
            core::mem::size_of::<Request>(),
        )
    };
    if wrote < 0 {
        return None;
    }

    // SAFETY: getpid takes nothing and cannot fail.
    let me = unsafe { libc::getpid() };
    let mut message = [0u8; 2048];
    for _ in 0..READS {
        let mut ready = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = libc::c_int::try_from(ANSWER.as_millis()).unwrap_or(libc::c_int::MAX);
        // SAFETY: one record, live for the call.
        if unsafe { libc::poll(&raw mut ready, 1, ms) } <= 0 {
            return None;
        }
        // SAFETY: a buffer of its own length, written by the call.
        let got = unsafe {
            libc::read(
                fd.as_raw_fd(),
                message.as_mut_ptr().cast(),
                core::mem::size_of_val(&message),
            )
        };
        let got = usize::try_from(got).ok()?;
        if let Some(answer) = answer(message.get(..got)?, me, seq) {
            return answer;
        }
    }
    None
}

/// The gateway a routing message names, when it is the answer to this
/// process's request `seq`: `None` for another message, `Some(None)` for the
/// answer with no IPv4 gateway in it.
fn answer(message: &[u8], me: libc::pid_t, seq: libc::c_int) -> Option<Option<Ipv4Addr>> {
    if message.len() < core::mem::size_of::<libc::rt_msghdr>() {
        return None;
    }
    // SAFETY: at least a header's bytes, read without regard to alignment.
    let header = unsafe { core::ptr::read_unaligned(message.as_ptr().cast::<libc::rt_msghdr>()) };
    if header.rtm_pid != me
        || header.rtm_seq != seq
        || libc::c_int::from(header.rtm_type) != libc::RTM_GET
    {
        return None;
    }
    if header.rtm_errno != 0 || header.rtm_flags & libc::RTF_GATEWAY == 0 {
        return Some(None);
    }
    Some(gateway_in(message, header.rtm_addrs))
}

/// The gateway among the addresses after a routing message's header: one for
/// each bit set in `addrs`, in the bits' order, each padded to four bytes and
/// a zero length taking four.
fn gateway_in(message: &[u8], addrs: libc::c_int) -> Option<Ipv4Addr> {
    let mut at = core::mem::size_of::<libc::rt_msghdr>();
    for bit in 0..libc::RTAX_MAX {
        let flag = 1 << bit;
        if addrs & flag == 0 {
            continue;
        }
        let len = usize::from(*message.get(at)?);
        if flag == libc::RTA_GATEWAY {
            let family = message.get(at + 1)?;
            if libc::c_int::from(*family) != libc::AF_INET || len < 8 {
                return None;
            }
            let octets: [u8; 4] = message.get(at + 4..at + 8)?.try_into().ok()?;
            return Some(Ipv4Addr::from(octets));
        }
        at += if len == 0 { 4 } else { len.div_ceil(4) * 4 };
    }
    None
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
    ///
    /// **Never a broken-pipe signal**: the stream is written with the
    /// standard library's calls, which pass no flag against one, and a write
    /// after the gateway resets would raise a signal whose disposition is the
    /// host process's.
    pub(crate) fn start(to: SocketAddrV4) -> io::Result<Self> {
        // SAFETY: plain arguments; the descriptor returned is owned below.
        let raw = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor this call opened and nothing else holds.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let on: libc::c_int = 1;
        let on_len =
            libc::socklen_t::try_from(core::mem::size_of_val(&on)).map_err(io::Error::other)?;
        // SAFETY: a live descriptor, and an integer option read during the
        // call only.
        let rc = unsafe {
            libc::setsockopt(
                raw,
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const on).cast(),
                on_len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // Non-blocking while it connects, and closed in a child the host
        // process starts; the system's socket call takes neither as a flag.
        // SAFETY: plain flag calls on a live descriptor.
        let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
        // SAFETY: as above.
        if flags < 0
            || unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            || unsafe { libc::fcntl(raw, libc::F_SETFD, libc::FD_CLOEXEC) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let address = sockaddr(to)?;
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
    // Longer than any host name the system holds, terminator included.
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

/// The machine's identifier, as the system keeps it, in its usual text. It
/// must not leave the machine as it is: the caller reduces it with a keyed
/// hash first. A sandboxed process may be refused it, and goes without.
pub(crate) fn machine_id() -> Option<Vec<u8>> {
    let mut id = [0u8; 16];
    let wait = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    // SAFETY: sixteen writable bytes and a timeout, both live for the call.
    if unsafe { libc::gethostuuid(id.as_mut_ptr(), &raw const wait) } != 0 {
        return None;
    }
    let hex: String = id.iter().map(|byte| format!("{byte:02X}")).collect();
    let text = [
        hex.get(..8)?,
        hex.get(8..12)?,
        hex.get(12..16)?,
        hex.get(16..20)?,
        hex.get(20..)?,
    ]
    .join("-");
    Some(text.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A routing message as the system writes one: the header, then a
    /// destination, a gateway and a mask whose length is zero.
    fn routed(flags: libc::c_int, addrs: libc::c_int, gateway: Ipv4Addr) -> Vec<u8> {
        // SAFETY: plain data; all zeroes is a valid value.
        let mut header: libc::rt_msghdr = unsafe { core::mem::zeroed() };
        header.rtm_type = u8::try_from(libc::RTM_GET).unwrap();
        header.rtm_pid = 7;
        header.rtm_seq = 1;
        header.rtm_flags = flags;
        header.rtm_addrs = addrs;
        // SAFETY: the header's own bytes.
        let mut message = unsafe {
            core::slice::from_raw_parts(
                (&raw const header).cast::<u8>(),
                core::mem::size_of::<libc::rt_msghdr>(),
            )
        }
        .to_vec();
        for address in [Ipv4Addr::UNSPECIFIED, gateway] {
            let sin = sockaddr(SocketAddrV4::new(address, 0)).unwrap();
            // SAFETY: the address's own bytes.
            message.extend_from_slice(unsafe {
                core::slice::from_raw_parts(
                    (&raw const sin).cast::<u8>(),
                    core::mem::size_of::<libc::sockaddr_in>(),
                )
            });
        }
        message.extend_from_slice(&[0, 0, 0, 0]);
        message
    }

    /// The answer to this process's request names its gateway, found after
    /// the destination in the message's own order; another process's message
    /// is not the answer, and a route with no gateway names none.
    #[test]
    fn the_answer_names_the_gateway_after_the_destination() {
        let gateway = Ipv4Addr::new(192, 168, 1, 1);
        let addrs = libc::RTA_DST | libc::RTA_GATEWAY | libc::RTA_NETMASK;
        let message = routed(libc::RTF_GATEWAY, addrs, gateway);
        assert_eq!(answer(&message, 7, 1), Some(Some(gateway)));
        assert_eq!(answer(&message, 8, 1), None, "another process's message");
        assert_eq!(answer(&message, 7, 2), None, "another request's answer");
        let direct = routed(0, addrs, gateway);
        assert_eq!(answer(&direct, 7, 1), Some(None), "a route with no gateway");
    }

    /// The connection to the gateway never raises a broken-pipe signal. The
    /// test runner ignores the signal, so a test cannot see one raised: the
    /// option is read back instead.
    #[test]
    fn the_connection_raises_no_broken_pipe_signal() {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let core::net::SocketAddr::V4(to) = listener.local_addr().unwrap() else {
            panic!("a v4 listener has a v4 address");
        };
        let connecting = Connecting::start(to).unwrap();
        let mut value: libc::c_int = 0;
        let mut len = libc::socklen_t::try_from(core::mem::size_of_val(&value)).unwrap();
        // SAFETY: a live descriptor, and an integer and its length written
        // during the call only.
        let rc = unsafe {
            libc::getsockopt(
                connecting.fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        assert_eq!(rc, 0);
        assert_eq!(value, 1, "a write after a reset would raise a signal");
    }

    /// A machine with a route beyond its network names its gateway, neither
    /// loopback nor nothing.
    #[test]
    fn the_route_beyond_the_network_names_its_gateway() {
        let beyond = SocketAddrV4::new(BEYOND, 9);
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
