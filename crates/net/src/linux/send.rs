//! Segmentation offload and source-pinned sends, as the kernel takes them.

use core::net::{IpAddr, SocketAddr};
use std::io;
use std::mem;

use super::socket::{Socket, to_storage};

/// Level and option for the segment size. Spelled out because the constant is
/// not exposed by every libc revision we build against.
const SOL_UDP: libc::c_int = 17;
const UDP_SEGMENT: libc::c_int = 103;

/// Width of the segment-size control value, in the kernel's size type.
const SEGMENT_FIELD: u32 = 2;
const _: () = assert!(SEGMENT_FIELD as usize == mem::size_of::<u16>());

/// Widths of the two packet-information structures, in the kernel's size
/// type, pinned the same way as the segment field.
const V4_SOURCE_FIELD: u32 = 12;
const _: () = assert!(V4_SOURCE_FIELD as usize == mem::size_of::<libc::in_pktinfo>());
const V6_SOURCE_FIELD: u32 = 20;
const _: () = assert!(V6_SOURCE_FIELD as usize == mem::size_of::<libc::in6_pktinfo>());

/// Control message storage, aligned as the kernel requires. Sized for the
/// segment size beside a v6 packet information: 24 plus 40 bytes of aligned
/// space, which is exactly this.
#[repr(align(8))]
struct Control([u8; 64]);

/// Whether a refused offload send says the kernel cannot segment, as opposed
/// to refusing this batch.
///
/// A closed set on purpose, and the default is transient: `EOPNOTSUPP` and
/// `EINVAL` are what a kernel or interface without segmentation answers, and
/// `EMSGSIZE` a batch past the segmentable maximum -- which the join bound
/// rules out, so if it arrives anyway the kernel is refusing the shape
/// itself. Everything else -- a full buffer, an interrupt, a policy or a
/// route refusing this destination -- is about the moment, not the
/// capability, and must not cost the fast path for the rest of the session.
pub(crate) fn offload_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::EINVAL | libc::EMSGSIZE)
    )
}

/// One syscall, many datagrams: the kernel splits `staged` every `segment`
/// bytes, and every one of them claims `from` when a source is pinned.
pub(crate) fn offload_send(
    socket: &Socket,
    staged: &[u8],
    segment: usize,
    to: SocketAddr,
    from: Option<IpAddr>,
) -> io::Result<()> {
    let (addr, addr_len) = to_storage(to);
    let segment = u16::try_from(segment).map_err(|_| io::Error::other("segment too large"))?;

    let mut control = Control([0u8; 64]);
    let mut iov = libc::iovec {
        iov_base: staged.as_ptr().cast_mut().cast(),
        iov_len: staged.len(),
    };
    // SAFETY: msghdr is plain data; zeroing it is the documented way to start.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_name = core::ptr::addr_of!(addr).cast_mut().cast();
    msg.msg_namelen = addr_len;
    msg.msg_iov = core::ptr::addr_of_mut!(iov);
    msg.msg_iovlen = 1;
    msg.msg_control = control.0.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE is a pure size computation over a constant.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(SEGMENT_FIELD) } as usize;
    if let Some(source) = from {
        // SAFETY: as above.
        msg.msg_controllen += unsafe { libc::CMSG_SPACE(source_field(source)) } as usize;
    }

    // SAFETY: the control buffer is aligned and sized for the segment size
    // beside one packet information, which is what msg_controllen reserved.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for the segment size"));
        }
        (*cmsg).cmsg_level = SOL_UDP;
        (*cmsg).cmsg_type = UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(SEGMENT_FIELD) as usize;
        core::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<u16>(), segment);

        if let Some(source) = from {
            let next = libc::CMSG_NXTHDR(&msg, cmsg);
            if next.is_null() {
                return Err(io::Error::other("no room for the source"));
            }
            write_source(next, source);
        }
    }

    // SAFETY: every pointer in `msg` refers to storage alive for this call.
    let sent = unsafe { libc::sendmsg(socket.raw(), &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One datagram claiming its own source address.
pub(crate) fn pinned_send(
    socket: &Socket,
    datagram: &[u8],
    to: SocketAddr,
    source: IpAddr,
) -> io::Result<()> {
    let (addr, addr_len) = to_storage(to);

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
    msg.msg_control = control.0.as_mut_ptr().cast();
    // SAFETY: CMSG_SPACE is a pure size computation over a constant.
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(source_field(source)) } as usize;

    // SAFETY: the control buffer is aligned and sized for one packet
    // information, which is what msg_controllen reserved.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for the source"));
        }
        write_source(cmsg, source);
    }

    // SAFETY: every pointer in `msg` refers to storage alive for this call.
    let sent = unsafe { libc::sendmsg(socket.raw(), &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
/// The v4 source goes into `ipi_spec_dst`: that is the field the kernel reads
/// on send, while `ipi_addr` is the receive-side field and stays zero. The
/// interface index is left zero in both families so routing still chooses the
/// interface; only the source address is claimed. A v4 source rides
/// `IPPROTO_IP` even on the dual-stack socket, because a v4-mapped destination
/// takes the v4 path and that path reads only its own level.
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
                (*cmsg).cmsg_len = libc::CMSG_LEN(V4_SOURCE_FIELD) as usize;
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
                (*cmsg).cmsg_len = libc::CMSG_LEN(V6_SOURCE_FIELD) as usize;
                let mut info = mem::zeroed::<libc::in6_pktinfo>();
                info.ipi6_addr.s6_addr = v6.octets();
                core::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in6_pktinfo>(), info);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set of errors that latch offload off, written out rather than
    /// derived, so a kind moving between the two classes fails here instead
    /// of agreeing with itself.
    #[test]
    fn only_capability_errors_disable_offload() {
        for capability in [libc::EOPNOTSUPP, libc::EINVAL, libc::EMSGSIZE] {
            assert!(
                offload_unsupported(&io::Error::from_raw_os_error(capability)),
                "{capability} must disable offload for the run"
            );
        }
        for transient in [
            libc::EAGAIN,
            libc::ENOBUFS,
            libc::EINTR,
            libc::EACCES,
            libc::ENETUNREACH,
        ] {
            assert!(
                !offload_unsupported(&io::Error::from_raw_os_error(transient)),
                "{transient} is about the moment and must not cost the fast path"
            );
        }
    }
}
