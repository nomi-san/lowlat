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
//!
//! The staging and every decision about a batch are here; the sends
//! themselves are the platform's (`crate::sys`).

use core::net::{IpAddr, SocketAddr};
use std::io;

use lowlat_core::conn::{Egress, Ttl};

use crate::socket::{DEFAULT_TTL, PROBE_TTL_MAX, Socket};
use crate::sys::{offload_send, offload_unsupported, pinned_send};

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

/// A staged burst headed for one destination.
pub(crate) struct Batch {
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
    pub(crate) fn new() -> Self {
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
    #[cfg(test)]
    fn offloading(&self) -> bool {
        self.offload
    }

    /// Datagrams the path has refused since the last one it took.
    #[cfg(test)]
    fn refused(&self) -> u64 {
        self.refused
    }

    /// How many datagrams are staged.
    pub(crate) fn staged(&self) -> usize {
        self.count
    }

    /// Room for the next datagram, at the offset it would occupy.
    ///
    /// Write into this, then hand the result to [`Batch::commit`]. An empty
    /// slice means the batch must be flushed first.
    pub(crate) fn stage(&mut self) -> &mut [u8] {
        self.buf.get_mut(self.used..).unwrap_or_default()
    }

    /// Accept the bytes just written into [`Batch::stage`].
    ///
    /// Flushes first when the datagram cannot join what is already staged,
    /// which is the common case at a size or destination change rather than an
    /// error.
    pub(crate) fn commit(&mut self, socket: &Socket, egress: Egress) -> io::Result<()> {
        // Bounded against the room the stage actually handed out, not the
        // whole buffer: the restart copy below spans exactly the claimed
        // length, and a miscounted emission must be refused here rather than
        // read past the buffer there.
        if egress.len == 0 || self.used + egress.len > SEND_BUF {
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
    pub(crate) fn flush(&mut self, socket: &Socket) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::Io;
    use crate::wake::Wake;
    use core::net::{IpAddr, Ipv6Addr};

    fn loopback_of(socket: &Socket) -> SocketAddr {
        let mut addr = socket.local_addr().expect("addr");
        addr.set_ip(IpAddr::V6(Ipv6Addr::LOCALHOST));
        addr
    }

    /// A socket to receive on, with the loop's own receive path around it.
    fn receiver() -> (Io, SocketAddr) {
        let socket = Socket::open(0).expect("receiver");
        let to = loopback_of(&socket);
        (Io::new(socket, Wake::new().expect("wake")), to)
    }

    /// Wait up to `timeout_ms` for the socket to speak, then take one batch;
    /// zero when nothing arrived in time.
    fn arrived(io: &mut Io, timeout_ms: f64) -> usize {
        if io.wait(timeout_ms).expect("wait").socket {
            io.drain().expect("drain")
        } else {
            0
        }
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
        let (mut receiver, local) = receiver();
        let to = SocketAddr::new(
            IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 20)),
            local.port(),
        );
        let source = IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 7));

        let mut batch = Batch::new();
        push_pinned(&mut batch, &sender, to, Ttl::Default, Some(source), b"pin");
        batch.flush(&sender).expect("flush");

        assert_eq!(arrived(&mut receiver, 1000.0), 1);
        let (from, _, bytes) = receiver.iter().next().expect("datagram");
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
        assert_eq!(arrived(&mut receiver, 1000.0), 1);
        let (from, _, bytes) = receiver.iter().next().expect("datagram");
        assert_eq!(bytes, b"free");
        assert_ne!(
            from.ip(),
            source,
            "the kernel default equals the pinned address, so the test is vacuous"
        );
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

    /// A length larger than the room the stage handed out is refused, not
    /// trusted. The copy that restarts a batch around a flush spans exactly
    /// the claimed length, so a miscounted emission would read past the
    /// buffer -- and a panic on the send path is the one answer worse than
    /// refusing the datagram.
    #[test]
    fn a_length_past_the_staged_room_is_refused() {
        let sender = Socket::open(0).expect("sender");
        let receiver = Socket::open(0).expect("receiver");
        let to = loopback_of(&receiver);
        let mut batch = Batch::new();

        for index in 0..63u8 {
            push(&mut batch, &sender, to, Ttl::Default, &[index; 1024]);
        }

        // A different destination forces the restart copy, and the claimed
        // length is more than the stage could have held.
        let lie = Egress {
            to: loopback_of(&sender),
            ttl: Ttl::Default,
            len: 2000,
            from: None,
        };
        assert!(
            batch.commit(&sender, lie).is_err(),
            "a length past the staged room was accepted"
        );
    }

    /// The join bound closes at what the kernel will segment, not at the
    /// staging buffer. Sixty-four kibibyte datagrams fill the buffer exactly
    /// and overrun the segmentable maximum; staged, that batch is refused
    /// whole -- and under a bound at the buffer size the refusal also cost
    /// offload for the rest of the session.
    #[test]
    fn a_batch_closes_at_the_segmentable_maximum() {
        let sender = Socket::open(0).expect("sender");
        let (mut receiver, to) = receiver();
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
        let mut got = 0;
        for _ in 0..20 {
            if got >= 63 {
                break;
            }
            got += arrived(&mut receiver, 200.0);
        }
        assert_eq!(got, 63, "the flushed burst did not arrive whole");
    }

    /// The pin survives segmentation offload: an offloaded burst carries the
    /// source beside the segment size, or a multi-homed host would keep the
    /// right source exactly until traffic got heavy enough to batch.
    #[test]
    fn an_offloaded_burst_keeps_its_pinned_source() {
        let sender = Socket::open(0).expect("sender");
        let (mut receiver, local) = receiver();
        let to = SocketAddr::new(
            IpAddr::V4(core::net::Ipv4Addr::new(127, 0, 0, 20)),
            local.port(),
        );
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

        let got = arrived(&mut receiver, 1000.0);
        assert_eq!(got, 8, "the burst did not arrive as eight datagrams");
        for (from, _, bytes) in receiver.iter() {
            assert_eq!(bytes.len(), 512);
            assert_eq!(from.ip(), source, "a segment lost the pinned source");
        }
    }

    /// The property the batch exists for: equal-size datagrams to one place
    /// leave together and arrive as separate datagrams.
    #[test]
    fn a_burst_leaves_together_and_arrives_separately() {
        let sender = Socket::open(0).expect("sender");
        let (mut receiver, to) = receiver();

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

        let got = arrived(&mut receiver, 1000.0);
        assert_eq!(got, 8, "the burst did not arrive as eight datagrams");
        for (index, (_, _, bytes)) in receiver.iter().enumerate() {
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
        let (mut receiver, to) = receiver();

        let mut batch = Batch::new();
        push(&mut batch, &sender, to, Ttl::Default, &[1u8; 400]);
        push(&mut batch, &sender, to, Ttl::Default, &[2u8; 400]);
        // Larger than the segment, so it cannot join and forces a flush.
        push(&mut batch, &sender, to, Ttl::Default, &[3u8; 900]);
        batch.flush(&sender).expect("flush");

        let mut seen = std::vec::Vec::new();
        let mut got = arrived(&mut receiver, 1000.0);
        while got > 0 {
            for (_, _, bytes) in receiver.iter() {
                seen.push((bytes[0], bytes.len()));
            }
            got = receiver.drain().expect("drain");
        }
        assert_eq!(seen, std::vec![(1u8, 400), (2u8, 400), (3u8, 900)]);
    }
}
