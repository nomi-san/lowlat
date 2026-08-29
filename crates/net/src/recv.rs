//! Batched receive: one syscall per burst, never one per datagram.
//!
//! A single outstanding receive plus a poll loses a keyframe burst outright.
//! On one platform that was the difference between zero and complete delivery
//! of a burst on loopback, so the batch is not an optimisation.
//!
//! Storage is allocated once and reused forever. The kernel writes straight
//! into the slots, so a received datagram costs no copy on our side and no
//! allocation on any path after construction.

use core::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::io;
use std::mem;
use std::os::fd::AsRawFd;

use crate::socket::{RECV_BATCH, RECV_SLOT, Socket, from_storage, socklen};

/// Room for one datagram's control messages: one packet-information
/// structure, well under this either way.
const CONTROL_LEN: usize = 64;

/// Control message storage, aligned as the kernel requires.
#[repr(align(8))]
#[derive(Clone, Copy)]
struct Control([u8; CONTROL_LEN]);

/// Reusable receive storage: slots, addresses, and the descriptors pointing at
/// them.
///
/// The message headers hold raw pointers into the boxed slot and address
/// arrays. Boxing is what makes that sound: moving a `Batch` moves the boxes,
/// not the heap allocations they point at, so the pointers stay valid for the
/// life of the object.
pub struct Batch {
    slots: Box<[[u8; RECV_SLOT]]>,
    names: Box<[libc::sockaddr_storage]>,
    /// Never read through this handle, and load bearing anyway: every message
    /// descriptor holds a raw pointer into it, so the field exists to keep the
    /// allocation alive. Removing it because nothing reads it would leave the
    /// kernel writing through dangling pointers.
    #[allow(dead_code, reason = "kept alive for the pointers in msgs")]
    iovs: Box<[libc::iovec]>,
    /// As `iovs`: the descriptors point into it, and the kernel writes each
    /// datagram's packet information there.
    #[allow(dead_code, reason = "kept alive for the pointers in msgs")]
    controls: Box<[Control]>,
    msgs: Box<[libc::mmsghdr]>,
    filled: usize,
}

impl core::fmt::Debug for Batch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Batch")
            .field("capacity", &self.slots.len())
            .field("filled", &self.filled)
            .finish()
    }
}

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
}

impl Batch {
    /// Allocate the slots. Once per session, never on a data path.
    pub fn new() -> Self {
        let mut slots = vec![[0u8; RECV_SLOT]; RECV_BATCH].into_boxed_slice();
        // SAFETY: sockaddr_storage and the descriptor structs are plain data
        // with no invalid bit patterns, so an all-zero value is valid.
        let mut names =
            vec![unsafe { mem::zeroed::<libc::sockaddr_storage>() }; RECV_BATCH].into_boxed_slice();
        let mut iovs = vec![unsafe { mem::zeroed::<libc::iovec>() }; RECV_BATCH].into_boxed_slice();
        let mut controls = vec![Control([0u8; CONTROL_LEN]); RECV_BATCH].into_boxed_slice();
        let mut msgs =
            vec![unsafe { mem::zeroed::<libc::mmsghdr>() }; RECV_BATCH].into_boxed_slice();

        for index in 0..RECV_BATCH {
            let Some(slot) = slots.get_mut(index) else {
                continue;
            };
            let base: *mut u8 = slot.as_mut_ptr();
            let Some(iov) = iovs.get_mut(index) else {
                continue;
            };
            iov.iov_base = base.cast();
            iov.iov_len = RECV_SLOT;

            let name: *mut libc::sockaddr_storage = match names.get_mut(index) {
                Some(name) => name,
                None => continue,
            };
            let iov_ptr: *mut libc::iovec = iov;
            let Some(control) = controls.get_mut(index) else {
                continue;
            };
            let control_ptr: *mut u8 = control.0.as_mut_ptr();
            let Some(msg) = msgs.get_mut(index) else {
                continue;
            };
            msg.msg_hdr.msg_iov = iov_ptr;
            msg.msg_hdr.msg_iovlen = 1;
            msg.msg_hdr.msg_name = name.cast();
            msg.msg_hdr.msg_control = control_ptr.cast();
            msg.msg_hdr.msg_controllen = CONTROL_LEN;
        }

        Self {
            slots,
            names,
            iovs,
            controls,
            msgs,
            filled: 0,
        }
    }

    /// Pull whatever the kernel has queued, up to the batch size.
    ///
    /// Returns how many datagrams arrived. Zero means the queue is drained; the
    /// caller stops when it sees a short batch, because a full one means there
    /// may be more behind it.
    pub fn drain(&mut self, socket: &Socket) -> io::Result<usize> {
        // The address and control lengths are in and out: the kernel
        // overwrites each with what it actually wrote, so a reused descriptor
        // that is not reset presents the previous datagram's lengths on the
        // next call and truncates both. Reset every slot, every pass.
        let name_len = socklen(mem::size_of::<libc::sockaddr_storage>());
        for msg in self.msgs.iter_mut() {
            msg.msg_hdr.msg_namelen = name_len;
            msg.msg_hdr.msg_controllen = CONTROL_LEN;
            msg.msg_len = 0;
        }

        // SAFETY: `msgs` is a contiguous array of exactly RECV_BATCH fully
        // initialised descriptors, each pointing at a slot and address this
        // object owns and keeps alive. MSG_DONTWAIT keeps the call from
        // blocking, so a caller that has already polled never parks here.
        let got = unsafe {
            libc::recvmmsg(
                socket.as_raw_fd(),
                self.msgs.as_mut_ptr(),
                libc::c_uint::try_from(RECV_BATCH).unwrap_or(1),
                libc::MSG_DONTWAIT,
                core::ptr::null_mut(),
            )
        };

        if got < 0 {
            let error = io::Error::last_os_error();
            return match error.kind() {
                // Drained, or interrupted before anything arrived. Neither is a
                // failure; both mean "nothing more this pass".
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => {
                    self.filled = 0;
                    Ok(0)
                }
                _ => Err(error),
            };
        }

        self.filled = usize::try_from(got).unwrap_or(0);
        Ok(self.filled)
    }

    /// True when the last drain filled every slot, so more may be queued.
    pub fn saturated(&self) -> bool {
        self.filled == RECV_BATCH
    }

    /// The datagrams from the last drain: the address each came from, the
    /// local address it arrived at, and the bytes.
    ///
    /// A v4-mapped source is reported as IPv4, structurally. The local
    /// address is `None` when the kernel attached no packet information,
    /// which a caller treats as "could not say" rather than an error.
    pub fn iter(&self) -> impl Iterator<Item = (SocketAddr, Option<IpAddr>, &[u8])> {
        (0..self.filled).filter_map(move |index| {
            let msg = self.msgs.get(index)?;
            let len = usize::try_from(msg.msg_len).unwrap_or(0);
            let bytes = self.slots.get(index)?.get(..len)?;
            let from = from_storage(self.names.get(index)?)?;
            Some((from, local_of(&msg.msg_hdr), bytes))
        })
    }
}

/// The local address a datagram arrived at, from its control messages.
///
/// The v4 answer arrives as `(IPPROTO_IP, IP_PKTINFO)` -- and `IP_PKTINFO`
/// is **8 on Linux**, not the 19 other platforms use -- while the v6 answer
/// arrives as `(IPPROTO_IPV6, IPV6_PKTINFO)` (50): `IPV6_RECVPKTINFO` (49)
/// switches delivery on but is not the type that arrives. On receive the
/// address is `ipi_addr`; `ipi6_spec_dst` does not exist and the v4
/// `ipi_spec_dst` is the send-side field, carrying the routing answer here.
fn local_of(msg: &libc::msghdr) -> Option<IpAddr> {
    // SAFETY: the descriptor's control pointer and length name storage this
    // batch owns, sized by the kernel to what it actually wrote; the CMSG
    // macros only compute offsets inside that region, and the reads are
    // unaligned-tolerant.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let level = (*cmsg).cmsg_level;
            let kind = (*cmsg).cmsg_type;
            if level == libc::IPPROTO_IP && kind == libc::IP_PKTINFO {
                let info =
                    core::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in_pktinfo>());
                return Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    info.ipi_addr.s_addr,
                ))));
            }
            if level == libc::IPPROTO_IPV6 && kind == libc::IPV6_PKTINFO {
                let info =
                    core::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<libc::in6_pktinfo>());
                let ip = Ipv6Addr::from(info.ipi6_addr.s6_addr);
                // Structural, as everywhere: a v4 arrival reported v4-mapped
                // is a v4 address.
                return Some(match ip.to_ipv4_mapped() {
                    Some(v4) => IpAddr::V4(v4),
                    None => IpAddr::V6(ip),
                });
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
        None
    }
}

// SAFETY: the raw pointers inside the message descriptors point only into this
// object's own boxed allocations, which move with it and are never aliased
// elsewhere. Nothing here is shared between threads without the usual
// borrowing rules, so a Batch is as sendable as the bytes it holds.
unsafe impl Send for Batch {}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{IpAddr, Ipv6Addr};

    fn loopback_of(socket: &Socket) -> SocketAddr {
        let mut addr = socket.local_addr().expect("addr");
        addr.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        addr
    }

    #[test]
    fn an_empty_socket_drains_to_nothing() {
        let socket = Socket::open(0).expect("open");
        let mut batch = Batch::new();
        assert_eq!(batch.drain(&socket).expect("drain"), 0);
        assert_eq!(batch.iter().count(), 0);
        assert!(!batch.saturated());
    }

    /// The property the batch exists for: a burst arrives whole, in one call,
    /// rather than one datagram per syscall with the rest dropped.
    #[test]
    fn a_burst_arrives_in_one_call() {
        let sender = Socket::open(0).expect("open sender");
        let receiver = Socket::open(0).expect("open receiver");
        let to = loopback_of(&receiver);

        let burst = 32;
        for index in 0..burst {
            let payload = [index as u8; 200];
            sender.send_to(&payload, to).expect("send");
        }
        assert!(receiver.wait_readable(1000.0).expect("poll"));

        let mut batch = Batch::new();
        let got = batch.drain(&receiver).expect("drain");
        assert_eq!(got, burst, "the burst did not arrive in one call");

        for (index, (_, _, bytes)) in batch.iter().enumerate() {
            assert_eq!(bytes.len(), 200);
            assert_eq!(bytes[0], index as u8, "datagrams arrived out of order");
        }
    }

    /// Reusing the batch must not carry the previous pass's address length
    /// forward, which is what an unreset descriptor does.
    #[test]
    fn a_reused_batch_reports_the_right_source_each_time() {
        let first = Socket::open(0).expect("open first");
        let second = Socket::open(0).expect("open second");
        let receiver = Socket::open(0).expect("open receiver");
        let to = loopback_of(&receiver);

        let mut batch = Batch::new();

        first.send_to(b"one", to).expect("send");
        assert!(receiver.wait_readable(1000.0).expect("poll"));
        assert_eq!(batch.drain(&receiver).expect("drain"), 1);
        let (from_first, _, bytes) = batch.iter().next().expect("one datagram");
        assert_eq!(bytes, b"one");
        assert_eq!(from_first.port(), loopback_of(&first).port());

        second.send_to(b"two", to).expect("send");
        assert!(receiver.wait_readable(1000.0).expect("poll"));
        assert_eq!(batch.drain(&receiver).expect("drain"), 1);
        let (from_second, _, bytes) = batch.iter().next().expect("one datagram");
        assert_eq!(bytes, b"two");
        assert_eq!(
            from_second.port(),
            loopback_of(&second).port(),
            "the second pass reported the first sender"
        );
    }

    /// The address a datagram arrived at is reported beside the address it
    /// came from. Any 127/8 destination reaches the same wildcard-bound
    /// socket, so two arrivals only differ by what the control message says
    /// -- which is exactly what a check answer needs to leave from the right
    /// address on a host that has more than one.
    #[test]
    fn the_address_a_datagram_arrived_at_is_reported() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let port = receiver.local_addr().expect("addr").port();
        let mut batch = Batch::new();

        let v4_dest = IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 11));
        sender
            .send_to(b"v4", SocketAddr::new(v4_dest, port))
            .expect("send");
        assert!(receiver.wait_readable(1000.0).expect("poll"));
        assert_eq!(batch.drain(&receiver).expect("drain"), 1);
        let (_, local, bytes) = batch.iter().next().expect("datagram");
        assert_eq!(bytes, b"v4");
        assert_eq!(
            local,
            Some(v4_dest),
            "the v4 arrival address was not reported"
        );

        let v6_dest = IpAddr::V6(Ipv6Addr::LOCALHOST);
        sender
            .send_to(b"v6", SocketAddr::new(v6_dest, port))
            .expect("send");
        assert!(receiver.wait_readable(1000.0).expect("poll"));
        assert_eq!(batch.drain(&receiver).expect("drain"), 1);
        let (_, local, bytes) = batch.iter().next().expect("datagram");
        assert_eq!(bytes, b"v6");
        assert_eq!(
            local,
            Some(v6_dest),
            "the v6 arrival address was not reported"
        );
    }

    /// A full-size datagram must survive, which it only does because the slot
    /// is sized from the protocol ceiling plus relay framing.
    #[test]
    fn a_full_size_datagram_survives() {
        let sender = Socket::open(0).expect("open sender");
        let receiver = Socket::open(0).expect("open receiver");
        let to = loopback_of(&receiver);

        let payload = vec![0xA5u8; lowlat_core::MAX_DATAGRAM];
        sender.send_to(&payload, to).expect("send");
        assert!(receiver.wait_readable(1000.0).expect("poll"));

        let mut batch = Batch::new();
        assert_eq!(batch.drain(&receiver).expect("drain"), 1);
        let (_, _, bytes) = batch.iter().next().expect("one datagram");
        assert_eq!(
            bytes.len(),
            lowlat_core::MAX_DATAGRAM,
            "a full-size datagram was truncated"
        );
    }
}
