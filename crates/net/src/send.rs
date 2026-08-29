//! Batched send: one syscall per burst where the kernel allows it.
//!
//! Segmentation offload takes one buffer and splits it into equal datagrams on
//! the way out, so a burst of full-size fragments costs one syscall instead of
//! one per fragment. That matters more as the datagram size rises, because the
//! packet rate falls while the burst size does not.
//!
//! The constraints come from the kernel and shape the API: every segment but
//! the last must be the same size, and all of them go to one destination. So a
//! batch closes when the size changes, the destination changes, or a datagram
//! needs a hop limit of its own.
//!
//! Offload is a fast path, never a requirement. A kernel that cannot segment
//! at all sends the whole run to a datagram per syscall, said once; a refusal
//! about one batch -- a full send buffer in the middle of a burst is the
//! ordinary case -- falls back for that batch alone, because trading the fast
//! path away forever on a transient is exactly backwards: the bursts that
//! fill the buffer are the ones segmentation exists for.

use core::net::{IpAddr, SocketAddr};
use std::io;
use std::mem;

use lowlat_core::conn::{Egress, Ttl};

use crate::socket::{DEFAULT_TTL, PROBE_TTL_MAX, Socket, to_storage};

/// Staging buffer size.
const SEND_BUF: usize = 64 * 1024;

/// Bytes the kernel will segment in one call: one maximal UDP payload, at
/// the v4 figure because it is the smaller of the two families -- 65,535
/// less 20 of IP header less 8 of UDP header. A batch staged past this is
/// refused whole, so the join bound closes here rather than at the buffer:
/// sixty-four kibibyte segments fit the buffer exactly and are one byte
/// ladder past what a single send may carry.
const OFFLOAD_MAX: usize = 65_507;
const _: () = assert!(OFFLOAD_MAX <= SEND_BUF);

/// Segments the kernel accepts in one offloaded send.
const MAX_SEGMENTS: usize = 64;

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

/// A staged burst headed for one destination.
pub struct Batch {
    buf: Box<[u8]>,
    /// Bytes staged so far.
    used: usize,
    /// Datagrams staged so far.
    count: usize,
    /// Size of every segment but the last.
    segment: usize,
    /// Set once a short datagram has been staged; nothing may follow it.
    closed: bool,
    to: Option<SocketAddr>,
    ttl: Ttl,
    /// The source the staged burst is pinned to, if any. Part of the batch
    /// key like the destination: one burst, one source.
    from: Option<IpAddr>,
    /// Cleared for good when the kernel says it cannot segment at all. A
    /// refusal about one batch -- a full buffer during a burst -- leaves it
    /// set, and that batch alone takes the ordinary path.
    offload: bool,
    /// Datagrams the path has refused since the last one it took.
    ///
    /// **A refusal is loss, not a fault**, so this exists to bound the logging
    /// rather than to gate anything: one line when the path starts refusing and
    /// one when it takes a datagram again, instead of one per datagram at
    /// datagram rates.
    refused: u64,
}

impl core::fmt::Debug for Batch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Batch")
            .field("count", &self.count)
            .field("used", &self.used)
            .field("offload", &self.offload)
            .field("refused", &self.refused)
            .finish()
    }
}

impl Default for Batch {
    fn default() -> Self {
        Self::new()
    }
}

impl Batch {
    /// Allocate the staging buffer. Once per session, never on a data path.
    pub fn new() -> Self {
        Self {
            buf: vec![0u8; SEND_BUF].into_boxed_slice(),
            used: 0,
            count: 0,
            segment: 0,
            closed: false,
            to: None,
            ttl: Ttl::Default,
            from: None,
            offload: true,
            refused: 0,
        }
    }

    /// Whether offload is still in use, or the kernel refused it.
    pub fn offloading(&self) -> bool {
        self.offload
    }

    /// Datagrams the path has refused since the last one it took.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// How many datagrams are staged.
    pub fn staged(&self) -> usize {
        self.count
    }

    /// Room for the next datagram, at the offset it would occupy.
    ///
    /// Write into this, then hand the result to [`Batch::commit`]. An empty
    /// slice means the batch must be flushed first.
    pub fn stage(&mut self) -> &mut [u8] {
        self.buf.get_mut(self.used..).unwrap_or_default()
    }

    /// Accept the bytes just written into [`Batch::stage`].
    ///
    /// Flushes first when the datagram cannot join what is already staged,
    /// which is the common case at a size or destination change rather than an
    /// error.
    pub fn commit(&mut self, socket: &Socket, egress: Egress) -> io::Result<()> {
        if egress.len == 0 || egress.len > SEND_BUF {
            return Err(io::Error::other("datagram outside the staging buffer"));
        }

        if self.joins(&egress) {
            if egress.len < self.segment {
                self.closed = true;
            }
            self.used += egress.len;
            self.count += 1;
        } else {
            // The staged bytes sit above what is about to be sent, so move them
            // down after the flush rather than asking the caller to write twice.
            let start = self.used;
            self.flush(socket)?;
            self.buf.copy_within(start..start + egress.len, 0);
            self.used = egress.len;
            self.count = 1;
            self.segment = egress.len;
            self.closed = false;
            self.to = Some(egress.to);
            self.ttl = egress.ttl;
            self.from = egress.from;
        }

        // A probe carries its own hop limit and must not be segmented with
        // anything else, so it leaves immediately.
        if egress.ttl != Ttl::Default {
            self.flush(socket)?;
        }
        Ok(())
    }

    /// Whether a datagram can join the staged burst.
    fn joins(&self, egress: &Egress) -> bool {
        if self.count == 0 {
            return false;
        }
        self.to == Some(egress.to)
            && self.ttl == egress.ttl
            && self.ttl == Ttl::Default
            && self.from == egress.from
            && !self.closed
            && self.count < MAX_SEGMENTS
            && self.used + egress.len <= OFFLOAD_MAX
            && egress.len <= self.segment
    }

    /// Send whatever is staged and reset.
    pub fn flush(&mut self, socket: &Socket) -> io::Result<()> {
        let (Some(to), true) = (self.to, self.count > 0) else {
            self.reset();
            return Ok(());
        };

        // A probe is emitted at a hop limit that cannot reach the peer, and the
        // socket is restored in the same breath. Leaving it lowered caps the
        // media path at a few hops, which presents as a path that establishes
        // and then carries nothing.
        let probe = self.ttl != Ttl::Default;
        if probe {
            socket.set_ttl(PROBE_TTL_MAX)?;
        }
        let result = self.transmit(socket, to);
        if probe {
            socket.set_ttl(DEFAULT_TTL)?;
        }

        self.reset();
        result
    }

    fn transmit(&mut self, socket: &Socket, to: SocketAddr) -> io::Result<()> {
        let Some(staged) = self.buf.get(..self.used) else {
            return Ok(());
        };

        if self.count > 1 && self.offload {
            match offload_send(socket, staged, self.segment, to, self.from) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if offload_unsupported(&error) {
                        // Not every kernel and interface pair will segment.
                        // Say so once, then take the ordinary path for the
                        // rest of the run rather than paying a failed syscall
                        // per burst.
                        self.offload = false;
                        lowlat_common::log_warn!(
                            "net: offload refused, per-datagram send from here, err={}",
                            error
                        );
                    }
                    // Anything else is about this batch or this moment -- a
                    // send buffer full mid-burst is the ordinary case -- so
                    // only these datagrams fall back and offload stays. The
                    // per-datagram sends below meet the same condition and
                    // `refused` keeps their logging bounded.
                }
            }
        }

        let mut at = 0;
        for _ in 0..self.count {
            let len = self.segment.min(self.used - at);
            let Some(datagram) = staged.get(at..at + len) else {
                break;
            };
            // **A datagram the path refuses is a datagram that was lost**, and
            // the protocol already recovers from loss. A link that has gone, a
            // route that has not come back and a local filter all surface here,
            // and none of them is a reason to tear down a session: a peer that
            // genuinely cannot be reached is ended by the delivery deadline,
            // which is evidence about the peer rather than about one syscall.
            let result = match self.from {
                Some(source) => pinned_send(socket, datagram, to, source),
                None => socket.send_to(datagram, to).map(|_| ()),
            };
            match result {
                Ok(()) => {
                    if self.refused > 0 {
                        lowlat_common::log_info!(
                            "net: path taking datagrams again, refused={}",
                            self.refused
                        );
                        self.refused = 0;
                    }
                }
                Err(error) => {
                    if self.refused == 0 {
                        lowlat_common::log_warn!(
                            "net: path refusing datagrams, dropped as loss, err={error}"
                        );
                    }
                    self.refused = self.refused.saturating_add(1);
                }
            }
            at += len;
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.used = 0;
        self.count = 0;
        self.segment = 0;
        self.closed = false;
        self.to = None;
        self.ttl = Ttl::Default;
        self.from = None;
    }
}

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
fn offload_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EOPNOTSUPP | libc::EINVAL | libc::EMSGSIZE)
    )
}

/// One syscall, many datagrams: the kernel splits `staged` every `segment`
/// bytes, and every one of them claims `from` when a source is pinned.
fn offload_send(
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
    let sent = unsafe { libc::sendmsg(socket_fd(socket), &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One datagram claiming its own source address.
fn pinned_send(socket: &Socket, datagram: &[u8], to: SocketAddr, source: IpAddr) -> io::Result<()> {
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
    let sent = unsafe { libc::sendmsg(socket_fd(socket), &msg, 0) };
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

fn socket_fd(socket: &Socket) -> libc::c_int {
    use std::os::fd::AsRawFd;
    socket.as_raw_fd()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recv;
    use core::net::{IpAddr, Ipv6Addr};

    fn loopback_of(socket: &Socket) -> SocketAddr {
        let mut addr = socket.local_addr().expect("addr");
        addr.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        addr
    }

    fn push(batch: &mut Batch, socket: &Socket, to: SocketAddr, ttl: Ttl, bytes: &[u8]) {
        push_pinned(batch, socket, to, ttl, None, bytes);
    }

    fn push_pinned(
        batch: &mut Batch,
        socket: &Socket,
        to: SocketAddr,
        ttl: Ttl,
        from: Option<IpAddr>,
        bytes: &[u8],
    ) {
        let room = batch.stage();
        room[..bytes.len()].copy_from_slice(bytes);
        batch
            .commit(
                socket,
                Egress {
                    to,
                    ttl,
                    len: bytes.len(),
                    from,
                },
            )
            .expect("commit");
    }

    /// **A datagram the path refuses is loss, not a fault.** Returning it as
    /// an error tears down whatever is driving the socket, and a loop that
    /// stops in the middle of a session leaves the host believing a guest is
    /// still connected. A path that refuses one datagram is usually carrying
    /// again a moment later, and a peer that genuinely cannot be reached is
    /// ended by the delivery deadline instead.
    #[test]
    fn a_refused_datagram_is_dropped_rather_than_returned() {
        let sender = Socket::open(0).expect("sender");
        let mut batch = Batch::default();

        // Port zero is refused outright, which is a real send failure that
        // needs no network state to produce.
        let mut nowhere = loopback_of(&sender);
        nowhere.set_port(0);
        push(&mut batch, &sender, nowhere, Ttl::Default, b"lost");
        batch.flush(&sender).expect("a refusal is not an error");
        assert_eq!(batch.refused(), 1);

        // And it clears when the path takes one again, which is what keeps an
        // outage to two log lines rather than one per datagram.
        let receiver = Socket::open(0).expect("receiver");
        push(
            &mut batch,
            &sender,
            loopback_of(&receiver),
            Ttl::Default,
            b"ok",
        );
        batch.flush(&sender).expect("flush");
        assert_eq!(batch.refused(), 0);
    }

    /// A pinned source is the address the peer sees. Any 127/8 address is a
    /// valid local source, so the pin is provable on one machine: unpinned,
    /// the kernel's own selection produces its default; pinned, the peer
    /// sees the address the datagram claimed.
    #[test]
    fn a_pinned_source_is_the_address_the_peer_sees() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let port = receiver.local_addr().expect("addr").port();
        let to = SocketAddr::new(IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 20)), port);
        let source = IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 7));

        let mut batch = Batch::new();
        push_pinned(&mut batch, &sender, to, Ttl::Default, Some(source), b"pin");
        batch.flush(&sender).expect("flush");

        assert!(receiver.wait_readable(1000.0).expect("poll"));
        let mut inbound = recv::Batch::new();
        assert_eq!(inbound.drain(&receiver).expect("drain"), 1);
        let (from, _, bytes) = inbound.iter().next().expect("datagram");
        assert_eq!(bytes, b"pin");
        assert_eq!(
            from.ip(),
            source,
            "the peer saw the kernel's source, not the pinned one"
        );

        // And the control: unpinned leaves from the kernel's own selection,
        // which is not the address above -- otherwise the assertion proves
        // nothing about the pin.
        push(&mut batch, &sender, to, Ttl::Default, b"free");
        batch.flush(&sender).expect("flush");
        assert!(receiver.wait_readable(1000.0).expect("poll"));
        assert_eq!(inbound.drain(&receiver).expect("drain"), 1);
        let (from, _, bytes) = inbound.iter().next().expect("datagram");
        assert_eq!(bytes, b"free");
        assert_ne!(
            from.ip(),
            source,
            "the kernel default equals the pinned address, so the test is vacuous"
        );
    }

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

    /// A refusal about one batch must not cost offload for the session. Only
    /// the kernel saying it cannot segment at all is permanent; a full send
    /// buffer mid-burst, an interrupt, or a destination the policy refuses
    /// are about the moment, and the burst falls back alone.
    #[test]
    fn a_batch_scoped_refusal_keeps_offload() {
        let sender = Socket::open(0).expect("sender");
        let mut batch = Batch::new();

        // Broadcast without SO_BROADCAST is refused at the syscall: a real
        // send failure that needs no network state and says nothing about
        // whether the kernel can segment.
        let to = SocketAddr::new(IpAddr::V4(core::net::Ipv4Addr::new(255, 255, 255, 255)), 9);
        push(&mut batch, &sender, to, Ttl::Default, &[1u8; 256]);
        push(&mut batch, &sender, to, Ttl::Default, &[2u8; 256]);
        batch.flush(&sender).expect("a refusal is not an error");

        assert!(
            batch.offloading(),
            "a refusal about one batch disabled offload for the session"
        );
        assert_eq!(batch.refused(), 2, "the fallback sends were not counted");
    }

    /// The join bound closes at what the kernel will segment, not at the
    /// staging buffer. Sixty-four kibibyte datagrams fill the buffer exactly
    /// and overrun the segmentable maximum; staged, that batch is refused
    /// whole -- and under a bound at the buffer size the refusal also cost
    /// offload for the rest of the session.
    #[test]
    fn a_batch_closes_at_the_segmentable_maximum() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let to = loopback_of(&receiver);
        let mut batch = Batch::new();

        for index in 0..64u8 {
            push(&mut batch, &sender, to, Ttl::Default, &[index; 1024]);
        }
        assert_eq!(
            batch.staged(),
            1,
            "a kibibyte datagram joined a batch the kernel cannot segment"
        );
        assert!(
            batch.offloading(),
            "the oversized batch reached the kernel and cost offload"
        );

        // And the sixty-three that flushed ahead of it all arrived.
        let mut inbound = recv::Batch::new();
        let mut got = 0;
        for _ in 0..20 {
            if got >= 63 {
                break;
            }
            if receiver.wait_readable(200.0).expect("poll") {
                got += inbound.drain(&receiver).expect("drain");
            }
        }
        assert_eq!(got, 63, "the flushed burst did not arrive whole");
    }

    /// The pin survives segmentation offload: an offloaded burst carries the
    /// source beside the segment size, or a multi-homed host would keep the
    /// right source exactly until traffic got heavy enough to batch.
    #[test]
    fn an_offloaded_burst_keeps_its_pinned_source() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let port = receiver.local_addr().expect("addr").port();
        let to = SocketAddr::new(IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 20)), port);
        let source = IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 7));

        let mut batch = Batch::new();
        for index in 0..8u8 {
            push_pinned(
                &mut batch,
                &sender,
                to,
                Ttl::Default,
                Some(source),
                &[index; 512],
            );
        }
        assert_eq!(batch.staged(), 8);
        batch.flush(&sender).expect("flush");
        assert!(
            batch.offloading(),
            "the kernel refused to segment, so this exercised the fallback"
        );

        assert!(receiver.wait_readable(1000.0).expect("poll"));
        let mut inbound = recv::Batch::new();
        let got = inbound.drain(&receiver).expect("drain");
        assert_eq!(got, 8, "the burst did not arrive as eight datagrams");
        for (from, _, bytes) in inbound.iter() {
            assert_eq!(bytes.len(), 512);
            assert_eq!(from.ip(), source, "a segment lost the pinned source");
        }
    }

    /// The property the batch exists for: equal-size datagrams to one place
    /// leave together and arrive as separate datagrams.
    #[test]
    fn a_burst_leaves_together_and_arrives_separately() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let to = loopback_of(&receiver);

        let mut batch = Batch::new();
        for index in 0..8u8 {
            push(&mut batch, &sender, to, Ttl::Default, &[index; 512]);
        }
        assert_eq!(batch.staged(), 8);
        batch.flush(&sender).expect("flush");

        // Without this the test passes identically on the fallback path, and
        // would keep passing if offload silently stopped working.
        assert!(
            batch.offloading(),
            "the kernel refused to segment, so this exercised the fallback"
        );

        assert!(receiver.wait_readable(1000.0).expect("poll"));
        let mut inbound = recv::Batch::new();
        let got = inbound.drain(&receiver).expect("drain");
        assert_eq!(got, 8, "the burst did not arrive as eight datagrams");
        for (index, (_, _, bytes)) in inbound.iter().enumerate() {
            assert_eq!(bytes.len(), 512);
            assert_eq!(bytes[0], index as u8);
        }
    }

    /// A short datagram closes the burst, because the kernel only allows the
    /// last segment to differ.
    #[test]
    fn a_short_datagram_closes_the_burst() {
        let socket = Socket::open(0).expect("socket");
        let to = loopback_of(&socket);
        let mut batch = Batch::new();

        push(&mut batch, &socket, to, Ttl::Default, &[1u8; 512]);
        push(&mut batch, &socket, to, Ttl::Default, &[2u8; 200]);
        assert_eq!(batch.staged(), 2);

        // Anything after the short one starts a new burst.
        push(&mut batch, &socket, to, Ttl::Default, &[3u8; 512]);
        assert_eq!(batch.staged(), 1, "a datagram followed a short segment");
    }

    #[test]
    fn a_new_destination_starts_a_new_burst() {
        let socket = Socket::open(0).expect("socket");
        let other = Socket::open(0).expect("other");
        let mut batch = Batch::new();

        push(
            &mut batch,
            &socket,
            loopback_of(&socket),
            Ttl::Default,
            &[1u8; 300],
        );
        push(
            &mut batch,
            &socket,
            loopback_of(&socket),
            Ttl::Default,
            &[1u8; 300],
        );
        assert_eq!(batch.staged(), 2);

        push(
            &mut batch,
            &socket,
            loopback_of(&other),
            Ttl::Default,
            &[2u8; 300],
        );
        assert_eq!(batch.staged(), 1, "a burst crossed destinations");
    }

    /// A probe leaves on its own and the socket is back at its normal hop limit
    /// afterwards. This is the shell-side half of the restore obligation.
    #[test]
    fn a_probe_leaves_alone_and_restores_the_hop_limit() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let to = loopback_of(&receiver);
        let mut batch = Batch::new();

        push(&mut batch, &sender, to, Ttl::Default, &[1u8; 300]);
        push(&mut batch, &sender, to, Ttl::Probe, &[2u8; 300]);

        assert_eq!(batch.staged(), 0, "the probe did not leave immediately");
        assert_eq!(
            sender.ttl().expect("ttl"),
            DEFAULT_TTL,
            "the socket was left at the probe hop limit"
        );
    }

    /// The bytes staged before a flush must not be lost or duplicated when the
    /// batch restarts around them.
    #[test]
    fn a_flush_mid_stage_keeps_every_datagram() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let to = loopback_of(&receiver);

        let mut batch = Batch::new();
        push(&mut batch, &sender, to, Ttl::Default, &[1u8; 400]);
        push(&mut batch, &sender, to, Ttl::Default, &[2u8; 400]);
        // Larger than the segment, so it cannot join and forces a flush.
        push(&mut batch, &sender, to, Ttl::Default, &[3u8; 900]);
        batch.flush(&sender).expect("flush");

        assert!(receiver.wait_readable(1000.0).expect("poll"));
        let mut inbound = recv::Batch::new();
        let mut seen = std::vec::Vec::new();
        loop {
            let got = inbound.drain(&receiver).expect("drain");
            if got == 0 {
                break;
            }
            for (_, _, bytes) in inbound.iter() {
                seen.push((bytes[0], bytes.len()));
            }
        }
        assert_eq!(seen, std::vec![(1u8, 400), (2u8, 400), (3u8, 900)]);
    }
}
