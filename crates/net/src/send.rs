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
//! Offload is a fast path, never a requirement. Where the kernel refuses it,
//! the batch falls back to a datagram per syscall permanently and says so once.

use core::net::SocketAddr;
use std::io;
use std::mem;

use lowlat_core::conn::{Egress, Ttl};

use crate::socket::{DEFAULT_TTL, PROBE_TTL_MAX, Socket, to_storage};

/// Staging capacity. The kernel will not segment more than this in one call.
const SEND_BUF: usize = 64 * 1024;

/// Segments the kernel accepts in one offloaded send.
const MAX_SEGMENTS: usize = 64;

/// Level and option for the segment size. Spelled out because the constant is
/// not exposed by every libc revision we build against.
const SOL_UDP: libc::c_int = 17;
const UDP_SEGMENT: libc::c_int = 103;

/// Width of the segment-size control value, in the kernel's size type.
const SEGMENT_FIELD: u32 = 2;
const _: () = assert!(SEGMENT_FIELD as usize == mem::size_of::<u16>());

/// Control message storage, aligned as the kernel requires.
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
    /// Cleared for good the first time the kernel refuses to segment.
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
            && !self.closed
            && self.count < MAX_SEGMENTS
            && self.used + egress.len <= SEND_BUF
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
            match offload_send(socket, staged, self.segment, to) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    // Not every kernel and interface pair will segment. Say so
                    // once, then take the ordinary path for the rest of the run
                    // rather than paying a failed syscall per burst.
                    self.offload = false;
                    lowlat_common::log_warn!(
                        "net: offload refused, per-datagram send from here, err={}",
                        error
                    );
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
            match socket.send_to(datagram, to) {
                Ok(_) => {
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
    }
}

/// One syscall, many datagrams: the kernel splits `staged` every `segment`
/// bytes.
fn offload_send(socket: &Socket, staged: &[u8], segment: usize, to: SocketAddr) -> io::Result<()> {
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

    // SAFETY: the control buffer is aligned and large enough for one control
    // message carrying a u16, which is what CMSG_SPACE above reserved.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("no room for the segment size"));
        }
        (*cmsg).cmsg_level = SOL_UDP;
        (*cmsg).cmsg_type = UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(SEGMENT_FIELD) as usize;
        core::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<u16>(), segment);
    }

    // SAFETY: every pointer in `msg` refers to storage alive for this call.
    let sent = unsafe { libc::sendmsg(socket_fd(socket), &msg, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
        let room = batch.stage();
        room[..bytes.len()].copy_from_slice(bytes);
        batch
            .commit(
                socket,
                Egress {
                    to,
                    ttl,
                    len: bytes.len(),
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
        for (index, (_, bytes)) in inbound.iter().enumerate() {
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
            for (_, bytes) in inbound.iter() {
                seen.push((bytes[0], bytes.len()));
            }
        }
        assert_eq!(seen, std::vec![(1u8, 400), (2u8, 400), (3u8, 900)]);
    }
}
