//! Sends as the system takes them: one message a datagram, carrying the
//! datagram's own mark and source.
//!
//! **No segmentation, and no batch in its place.** The system cannot split one
//! buffer into datagrams, and its batched send measured no cheaper per
//! datagram than a send each -- 4.2 us either way on the development Mac, 7
//! percent less at 64 a call, under 2 at the one or two a client's pass
//! stages -- so the shared batch starts with offload off here and sends a
//! datagram a call.
//!
//! **A v4 send carries its own mark.** A v4-mapped destination takes the
//! system's v4 path, which reads only v4-level control messages and not the
//! socket's v6 class; the v4-level options themselves are refused on a
//! dual-stack socket, so the type of service rides each v4 send.

use core::net::{IpAddr, SocketAddr};
use std::io;
use std::mem;

use super::socket::{Socket, to_storage};
use crate::socket::DSCP_EF;

/// Whether the shared batch segments a run of datagrams in one call. Not here.
pub(crate) const OFFLOAD: bool = false;

/// Widths of the control values, in the system's size type, pinned to the
/// structures they carry.
const TOS_FIELD: u32 = 4;
const _: () = assert!(TOS_FIELD as usize == mem::size_of::<libc::c_int>());
const V4_SOURCE_FIELD: u32 = 12;
const _: () = assert!(V4_SOURCE_FIELD as usize == mem::size_of::<libc::in_pktinfo>());
const V6_SOURCE_FIELD: u32 = 20;
const _: () = assert!(V6_SOURCE_FIELD as usize == mem::size_of::<libc::in6_pktinfo>());

/// Control message storage, aligned as the system requires: room for the
/// type of service beside a source, checked below.
#[repr(align(8))]
struct Control([u8; 64]);

/// A refusal from [`offload_send`], which says the system cannot segment.
pub(crate) fn offload_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
}

/// Never reached: the batch starts with offload off here ([`OFFLOAD`]). It
/// answers as a system that cannot segment would, should it be.
pub(crate) fn offload_send(
    _socket: &Socket,
    _staged: &[u8],
    _segment: usize,
    _to: SocketAddr,
    _from: Option<IpAddr>,
) -> io::Result<()> {
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

/// One datagram claiming its own source address.
pub(crate) fn pinned_send(
    socket: &Socket,
    datagram: &[u8],
    to: SocketAddr,
    source: IpAddr,
) -> io::Result<()> {
    send(socket, datagram, to, Some(source)).map(|_| ())
}

/// One datagram to `to`, marked, and claiming `source` where one is given.
pub(super) fn send(
    socket: &Socket,
    datagram: &[u8],
    to: SocketAddr,
    source: Option<IpAddr>,
) -> io::Result<usize> {
    let (addr, addr_len) = to_storage(to);
    let v4 = match to {
        SocketAddr::V4(_) => true,
        SocketAddr::V6(v6) => v6.ip().to_ipv4_mapped().is_some(),
    };

    let mut control = Control([0u8; 64]);
    let mut iov = libc::iovec {
        iov_base: datagram.as_ptr().cast_mut().cast(),
        iov_len: datagram.len(),
    };
    // SAFETY: msghdr is plain data; zeroing it is the documented way to start.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = core::ptr::addr_of!(addr).cast_mut().cast();
    msg.msg_namelen = addr_len;
    msg.msg_iov = core::ptr::addr_of_mut!(iov);
    msg.msg_iovlen = 1;
    let mut control_len = 0;
    if v4 {
        // SAFETY: CMSG_SPACE is a pure size computation over a constant.
        control_len += unsafe { libc::CMSG_SPACE(TOS_FIELD) };
    }
    if let Some(source) = source {
        // SAFETY: as above.
        control_len += unsafe { libc::CMSG_SPACE(source_field(source)) };
    }
    if control_len > 0 {
        msg.msg_control = control.0.as_mut_ptr().cast();
        msg.msg_controllen = control_len;

        // SAFETY: the control buffer is aligned, and msg_controllen reserves
        // CMSG_SPACE for exactly the messages written below, in that order.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            if v4 {
                if cmsg.is_null() {
                    return Err(io::Error::other("no room for the mark"));
                }
                (*cmsg).cmsg_level = libc::IPPROTO_IP;
                (*cmsg).cmsg_type = libc::IP_TOS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(TOS_FIELD);
                core::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::c_int>(), DSCP_EF);
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
            if let Some(source) = source {
                if cmsg.is_null() {
                    return Err(io::Error::other("no room for the source"));
                }
                write_source(cmsg, source);
            }
        }
    }

    // SAFETY: every pointer in `msg` refers to storage alive for this call.
    let sent = unsafe { libc::sendmsg(socket.raw(), &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(usize::try_from(sent).unwrap_or(0))
}

/// The width of the packet information carrying `source`.
fn source_field(source: IpAddr) -> u32 {
    match source {
        IpAddr::V4(_) => V4_SOURCE_FIELD,
        IpAddr::V6(_) => V6_SOURCE_FIELD,
    }
}

/// Fill one control message with the source address.
///
/// The v4 source goes into `ipi_spec_dst` at the v4 level: a v4-mapped
/// destination takes the v4 path, which leaves a v6 packet information
/// unread and sends from the system's own choice. The interface index is
/// left zero in both families, so routing still chooses the interface; only
/// the address is claimed.
///
/// # Safety
///
/// `cmsg` must point into a control buffer with `CMSG_SPACE` room for the
/// structure the source's family selects.
unsafe fn write_source(cmsg: *mut libc::cmsghdr, source: IpAddr) {
    match source {
        IpAddr::V4(v4) => {
            // SAFETY: the caller reserved room for an in_pktinfo.
            unsafe {
                (*cmsg).cmsg_level = libc::IPPROTO_IP;
                (*cmsg).cmsg_type = libc::IP_PKTINFO;
                (*cmsg).cmsg_len = libc::CMSG_LEN(V4_SOURCE_FIELD);
                let mut info = mem::zeroed::<libc::in_pktinfo>();
                info.ipi_spec_dst.s_addr = u32::from(v4).to_be();
                core::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in_pktinfo>(), info);
            }
        }
        IpAddr::V6(v6) => {
            // SAFETY: the caller reserved room for an in6_pktinfo.
            unsafe {
                (*cmsg).cmsg_level = libc::IPPROTO_IPV6;
                (*cmsg).cmsg_type = libc::IPV6_PKTINFO;
                (*cmsg).cmsg_len = libc::CMSG_LEN(V6_SOURCE_FIELD);
                let mut info = mem::zeroed::<libc::in6_pktinfo>();
                info.ipi6_addr.s6_addr = v6.octets();
                core::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in6_pktinfo>(), info);
            }
        }
    }
}

// The control buffer holds the widest pair this module writes.
// SAFETY: CMSG_SPACE is a pure size computation over constants.
const _: () = assert!(
    unsafe { libc::CMSG_SPACE(TOS_FIELD) + libc::CMSG_SPACE(V4_SOURCE_FIELD) } as usize
        <= mem::size_of::<Control>()
);
