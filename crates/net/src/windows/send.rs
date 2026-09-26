//! Segmentation offload and source-pinned sends, as the system takes them:
//! one message send carrying the segment size, the claimed source, or both,
//! as control messages.
//!
//! Synchronous, with no overlapped structure, so a send never posts an entry
//! to the loop's completion port; only receives and the wake do.

use core::net::{IpAddr, SocketAddr};
use std::io;
use std::mem;

use windows_sys::Win32::Networking::WinSock as ws;

use super::control;
use super::socket::{Socket, last_error, to_storage};

/// Room for the segment size beside a v6 packet information, each with its
/// header and padding.
const CONTROL_LEN: usize =
    control::space(mem::size_of::<u32>()) + control::space(mem::size_of::<ws::IN6_PKTINFO>());

/// Control message storage, aligned as the system requires.
#[repr(align(8))]
struct Control([u8; CONTROL_LEN]);

/// Whether a refused offload send says the system cannot segment, as opposed
/// to refusing this batch.
///
/// A closed set on purpose, and the default is transient: `WSAEOPNOTSUPP`,
/// `WSAENOPROTOOPT` and `WSAEINVAL` are what a stack without segmentation
/// answers the segment size with, and `WSAEMSGSIZE` a batch past the
/// segmentable maximum -- which the join bound rules out, so if it arrives
/// anyway the system is refusing the shape itself. Everything else -- a full
/// buffer, an interrupt, a destination the policy refuses -- is about the
/// moment, not the capability, and must not cost the fast path for the rest
/// of the session.
pub(crate) fn offload_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(ws::WSAEOPNOTSUPP | ws::WSAENOPROTOOPT | ws::WSAEINVAL | ws::WSAEMSGSIZE)
    )
}

/// One call, many datagrams: the system splits `staged` every `segment` bytes,
/// and every one of them claims `from` when a source is pinned.
pub(crate) fn offload_send(
    socket: &Socket,
    staged: &[u8],
    segment: usize,
    to: SocketAddr,
    from: Option<IpAddr>,
) -> io::Result<()> {
    let segment = u32::try_from(segment).map_err(|_| io::Error::other("segment too large"))?;
    let mut control = Control([0u8; CONTROL_LEN]);
    let mut used = control::put(
        &mut control.0,
        0,
        ws::IPPROTO_UDP,
        ws::UDP_SEND_MSG_SIZE,
        segment,
    )
    .ok_or_else(|| io::Error::other("no room for the segment size"))?;
    if let Some(source) = from {
        used = put_source(&mut control.0, used, source)
            .ok_or_else(|| io::Error::other("no room for the source"))?;
    }
    send(
        socket,
        staged,
        to,
        control.0.get_mut(..used).unwrap_or_default(),
    )
}

/// One datagram claiming its own source address.
pub(crate) fn pinned_send(
    socket: &Socket,
    datagram: &[u8],
    to: SocketAddr,
    source: IpAddr,
) -> io::Result<()> {
    let mut control = Control([0u8; CONTROL_LEN]);
    let used = put_source(&mut control.0, 0, source)
        .ok_or_else(|| io::Error::other("no room for the source"))?;
    send(
        socket,
        datagram,
        to,
        control.0.get_mut(..used).unwrap_or_default(),
    )
}

/// Write the source a datagram claims.
///
/// Here the address field is the source on send, in both families; the
/// interface index stays zero so routing still chooses the interface and only
/// the address is claimed. A v4 source rides the v4 level even on the
/// dual-stack socket, because a v4-mapped destination takes the v4 path.
///
/// **The source must be an address this host holds.** One it does not is
/// refused (`WSAEINVAL`), and on this platform the loopback holds 127.0.0.1
/// and not the rest of 127/8.
fn put_source(buf: &mut [u8], at: usize, source: IpAddr) -> Option<usize> {
    match source {
        IpAddr::V4(v4) => control::put(
            buf,
            at,
            ws::IPPROTO_IP,
            ws::IP_PKTINFO,
            ws::IN_PKTINFO {
                ipi_addr: ws::IN_ADDR {
                    S_un: ws::IN_ADDR_0 {
                        S_addr: u32::from_ne_bytes(v4.octets()),
                    },
                },
                ipi_ifindex: 0,
            },
        ),
        IpAddr::V6(v6) => control::put(
            buf,
            at,
            ws::IPPROTO_IPV6,
            ws::IPV6_PKTINFO,
            ws::IN6_PKTINFO {
                ipi6_addr: ws::IN6_ADDR {
                    u: ws::IN6_ADDR_0 { Byte: v6.octets() },
                },
                ipi6_ifindex: 0,
            },
        ),
    }
}

/// One message send of `bytes` to `to`, carrying `control`.
fn send(socket: &Socket, bytes: &[u8], to: SocketAddr, control: &mut [u8]) -> io::Result<()> {
    let (addr, addr_len) = to_storage(to);
    let mut buffer = ws::WSABUF {
        len: u32::try_from(bytes.len()).map_err(|_| io::Error::other("send too large"))?,
        // The system reads through this pointer and never writes: the send
        // path takes the buffer by a mutable pointer only for its signature.
        buf: bytes.as_ptr().cast_mut(),
    };
    let msg = ws::WSAMSG {
        name: (&raw const addr).cast_mut().cast(),
        namelen: addr_len,
        lpBuffers: &raw mut buffer,
        dwBufferCount: 1,
        Control: ws::WSABUF {
            len: u32::try_from(control.len()).unwrap_or(0),
            buf: control.as_mut_ptr(),
        },
        dwFlags: 0,
    };
    let mut sent: u32 = 0;
    // SAFETY: every pointer in `msg` refers to storage alive for this call,
    // which is synchronous: no overlapped structure and no completion routine.
    let rc = unsafe {
        ws::WSASendMsg(
            socket.raw(),
            &raw const msg,
            0,
            &raw mut sent,
            core::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 {
        return Err(last_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set of errors that latch offload off, written out rather than
    /// derived, so a kind moving between the two classes fails here instead
    /// of agreeing with itself.
    #[test]
    fn only_capability_errors_disable_offload() {
        for capability in [
            ws::WSAEOPNOTSUPP,
            ws::WSAENOPROTOOPT,
            ws::WSAEINVAL,
            ws::WSAEMSGSIZE,
        ] {
            assert!(
                offload_unsupported(&io::Error::from_raw_os_error(capability)),
                "{capability} must disable offload for the run"
            );
        }
        for transient in [
            ws::WSAEWOULDBLOCK,
            ws::WSAENOBUFS,
            ws::WSAEINTR,
            ws::WSAEACCES,
            ws::WSAENETUNREACH,
            ws::WSAEHOSTUNREACH,
        ] {
            assert!(
                !offload_unsupported(&io::Error::from_raw_os_error(transient)),
                "{transient} is about the moment and must not cost the fast path"
            );
        }
    }

    /// The storage holds the largest pair written into it.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn the_control_storage_holds_a_segment_size_beside_a_v6_source() {
        assert_eq!(CONTROL_LEN, 24 + 40);
    }
}
