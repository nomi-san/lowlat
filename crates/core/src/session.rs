//! The sans-IO session: bytes in, bytes out, time as a parameter.
//!
//! This is the whole protocol core behind one object. It reads no clock, owns
//! no socket, spawns no thread, and allocates nothing. The shell drives it:
//!
//! ```text
//! loop:
//!     timeout = session.next_timer_ms(now)
//!     wait for a packet, an application send, or that timeout
//!     for each datagram:  session.process_input(bytes, now)
//!     drain:              while let Some(n) = session.get_output(now, buf) { send(buf[..n]) }
//! ```
//!
//! Storage for the per-channel rings is lent by the caller, so a session that
//! carries only control and video costs two rings rather than nineteen.

use crate::channel::{Drops, RecvRing, Stored};
use crate::envelope::{ENVELOPE_LEN, Envelope};
use crate::error::{Error, Result};
use crate::message::Message;
use crate::packet::{self, Ack, AckKind, CHANNEL_COUNT, Packet};
use crate::send::SendRing;
use crate::seq;

/// Longest gap between group acknowledgements while a session is alive.
pub const ACK_CADENCE_MS: f64 = 30.0;
/// Shortest gap between data-driven group acknowledgements. An arrival ends
/// the wait early only when it reveals a gap or ends a message; anything else
/// rides the next acknowledgement of either kind, whose cumulative counts
/// cover it. One timestamp serves both floors: every acknowledgement sent
/// resets the clock for each.
const ACK_DATA_FLOOR_MS: f64 = 10.0;
/// No progress for this long is a soft failure.
pub const LIVENESS_SOFT_MS: f64 = 60_000.0;
/// No progress for this long is a hard failure.
pub const LIVENESS_HARD_MS: f64 = 120_000.0;
/// Data outstanding with none of it acknowledged for this long is a hard
/// failure of its own.
///
/// **A different question from the two deadlines above, and a much shorter
/// one.** Those ask whether anything arrives, which a peer that keeps
/// acknowledging on the cadence while receiving nothing satisfies for ever;
/// everything queued for it is retransmitted for exactly as long. A congested
/// path recovers inside a few seconds and acknowledges throughout, so a window
/// that has moved by nothing in fifteen is not congestion.
pub const DELIVERY_DEADLINE_MS: f64 = 15_000.0;

/// Weight given to a new round-trip sample.
const SRTT_ALPHA: f64 = 0.1;

/// What a datagram turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    /// Data arrived on a channel we hold a ring for. The ring may still have
    /// refused the store; [`Session::recv_drops`] carries the counts.
    Data { channel: u8 },
    /// An acknowledgement, which may have advanced windows.
    Ack,
    /// A keepalive.
    Keepalive,
    /// Well formed but not for a channel we hold a ring for.
    Unhandled { channel: u8 },
}

/// How the session is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Alive,
    /// No progress for [`LIVENESS_SOFT_MS`].
    Stalled,
    /// No progress for [`LIVENESS_HARD_MS`]. Tear down.
    Dead,
    /// Data has been outstanding and unacknowledged for
    /// [`DELIVERY_DEADLINE_MS`]. Tear down.
    ///
    /// **Distinct from [`Health::Dead`] because the cause is.** Dead is a peer
    /// that says nothing; this is a peer that still speaks and has stopped
    /// receiving, and the two are told apart only by whether a window moves.
    Undeliverable,
}

/// One channel's delivery progress.
#[derive(Debug, Clone, Copy)]
struct Delivery {
    /// The channel's acknowledged count when it last moved.
    acked_seen: u64,
    /// When that was. **Seeded at construction and refreshed by every poll
    /// that finds the channel empty**, which is what a ring attached after
    /// construction relies on: nothing can be outstanding on it before it has
    /// been given something to send.
    since_ms: f64,
}

/// One peer-to-peer session.
#[derive(Debug)]
pub struct Session<'a> {
    envelope: Envelope,
    recv: [Option<RecvRing<'a>>; CHANNEL_COUNT],
    send: [Option<SendRing<'a>>; CHANNEL_COUNT],
    level: usize,

    /// Monotonic per sender. Never reused: a wrap would repeat a nonce.
    tx_counter: u64,
    srtt_ms: f64,
    srtt_seeded: bool,

    last_ack_sent_ms: f64,
    last_progress_ms: f64,
    /// Per channel, when delivery on it last made progress.
    ///
    /// **Per channel and not per session.** A peer that has stopped draining
    /// one ring keeps acknowledging the others, and it is the busy channel
    /// that backs up: a figure summed across all of them is refreshed by the
    /// cheap traffic and never reports the expensive traffic going nowhere.
    delivery: [Delivery; CHANNEL_COUNT],
    ack_due: bool,
    /// Why the pending acknowledgement is owed. An accepted store makes it an
    /// acknowledgement; the cadence alone makes it a keepalive.
    ack_kind: AckKind,
    /// The accepted fragment the next acknowledgement names.
    trigger: (u8, u32),
    /// Whether that fragment revealed a gap on its own channel.
    trigger_nack: bool,

    /// Which channel the output drain is working through.
    drain_channel: usize,
    drain_started: bool,

    /// Set once the peer reports the full channel count, which opens the send
    /// windows from the shallow-ring floor to the current generation's depth.
    peer_identified: bool,
}

impl<'a> Session<'a> {
    /// Build a session. Rings are attached separately, per channel.
    pub fn new(envelope: Envelope, level: usize, now_ms: f64) -> Self {
        Self {
            envelope,
            recv: core::array::from_fn(|_| None),
            send: core::array::from_fn(|_| None),
            level,
            tx_counter: 0,
            srtt_ms: 0.0,
            srtt_seeded: false,
            last_ack_sent_ms: now_ms,
            last_progress_ms: now_ms,
            delivery: [Delivery {
                acked_seen: 0,
                since_ms: now_ms,
            }; CHANNEL_COUNT],
            ack_due: false,
            ack_kind: AckKind::Ack,
            trigger: (0, 0),
            trigger_nack: false,
            drain_channel: 0,
            drain_started: false,
            peer_identified: false,
        }
    }

    /// Give the session a receive ring for `channel`.
    pub fn attach_recv(&mut self, channel: u8, ring: RecvRing<'a>) -> Result<()> {
        *self
            .recv
            .get_mut(channel as usize)
            .ok_or(Error::Malformed)? = Some(ring);
        Ok(())
    }

    /// Give the session a send ring for `channel`.
    pub fn attach_send(&mut self, channel: u8, ring: SendRing<'a>) -> Result<()> {
        *self
            .send
            .get_mut(channel as usize)
            .ok_or(Error::Malformed)? = Some(ring);
        Ok(())
    }

    /// Smoothed round trip, in fractional milliseconds.
    pub fn srtt_ms(&self) -> f64 {
        self.srtt_ms
    }

    /// Liveness, judged against the last forward progress in each direction.
    ///
    /// **Both directions, because a session can fail in either.** Nothing
    /// arriving is one failure; everything queued sitting unacknowledged is
    /// another, and it is invisible to a deadline that only watches what comes
    /// in.
    pub fn health(&self, now_ms: f64) -> Health {
        let idle = now_ms - self.last_progress_ms;
        if idle >= LIVENESS_HARD_MS {
            return Health::Dead;
        }
        if self.undeliverable(now_ms) {
            return Health::Undeliverable;
        }
        if idle >= LIVENESS_SOFT_MS {
            Health::Stalled
        } else {
            Health::Alive
        }
    }

    /// True when a channel has held data the peer has not acknowledged for the
    /// whole of [`DELIVERY_DEADLINE_MS`].
    ///
    /// The window is read now rather than remembered, so a channel that
    /// drained a moment ago is not judged on what it used to hold.
    fn undeliverable(&self, now_ms: f64) -> bool {
        self.send.iter().enumerate().any(|(channel, ring)| {
            ring.as_ref().is_some_and(|ring| {
                ring.in_flight() > 0
                    && self
                        .delivery
                        .get(channel)
                        .is_some_and(|entry| now_ms - entry.since_ms >= DELIVERY_DEADLINE_MS)
            })
        })
    }

    /// One channel's send pressure: the outstanding window, the stale count
    /// from the last scan, and the payload bytes sent so far.
    ///
    /// **The window is `send_next - send_base`**, which is what both the
    /// congestion controller and the delivery gate's room test are defined
    /// against. It is not [`crate::send::SendRing::outstanding`], which counts
    /// what a single scan released and is bounded by the per-channel cap.
    pub fn send_pressure(&self, channel: u8) -> Option<(u32, u32, u64)> {
        let ring = self.send.get(channel as usize)?.as_ref()?;
        Some((ring.in_flight(), ring.stale(), ring.bytes_sent()))
    }

    /// Contiguous frontier on `channel`: what we would acknowledge.
    pub fn recv_cumulative(&self, channel: u8) -> Option<u32> {
        Some(self.recv.get(channel as usize)?.as_ref()?.cumulative_ack())
    }

    /// Stores the ring on `channel` refused, counted per kind.
    pub fn recv_drops(&self, channel: u8) -> Option<Drops> {
        Some(self.recv.get(channel as usize)?.as_ref()?.drops())
    }

    /// Anchor a receive channel at `sequence`.
    ///
    /// For a session joined mid-stream, or a replay that does not begin at
    /// zero. Discards anything already buffered on that channel.
    pub fn reset_recv(&mut self, channel: u8, sequence: u32) -> Result<()> {
        self.recv
            .get_mut(channel as usize)
            .and_then(Option::as_mut)
            .ok_or(Error::Malformed)?
            .reset_to(sequence);
        Ok(())
    }

    /// Queue a message for sending on `channel`.
    ///
    /// Nothing goes on the wire here. The fragments become pending and
    /// [`Session::get_output`] releases them, so backpressure is visible as a
    /// refusal rather than as unbounded buffering.
    pub fn send_message(&mut self, channel: u8, header: &[u8], payload: &[u8]) -> Result<u32> {
        let ring = self
            .send
            .get_mut(channel as usize)
            .and_then(Option::as_mut)
            .ok_or(Error::Malformed)?;
        let message = Message::new(header, payload)?;
        ring.enqueue(&message)
    }

    /// Take the next complete message from `channel`, if one has arrived.
    pub fn take_message(&mut self, channel: u8, out: &mut [u8]) -> Option<Result<usize>> {
        self.recv
            .get_mut(channel as usize)
            .and_then(Option::as_mut)?
            .take_message(out)
    }

    /// True if `channel` is missing a fragment below what has arrived.
    pub fn has_gap(&self, channel: u8) -> bool {
        self.recv
            .get(channel as usize)
            .and_then(Option::as_ref)
            .is_some_and(RecvRing::has_gap)
    }

    /// Abandon an unfillable gap on `channel` and resume further along.
    ///
    /// Policy lives with the caller, deliberately. Only the layer that
    /// understands the payload can say which slots are resumable, and only the
    /// shell knows how long a stall has lasted. The core supplies the
    /// mechanism and the guarantee that the jump goes to the furthest usable
    /// slot rather than the nearest.
    pub fn escape_stall(&mut self, channel: u8, resumable: impl Fn(&[u8]) -> bool) -> Option<u32> {
        self.recv
            .get_mut(channel as usize)
            .and_then(Option::as_mut)?
            .escape_stall(resumable)
    }

    /// Feed one received datagram.
    pub fn process_input(
        &mut self,
        datagram: &[u8],
        now_ms: f64,
        scratch: &mut [u8],
    ) -> Result<Inbound> {
        let opened = self.envelope.open(datagram, scratch)?;
        let packet = packet::parse(opened.cleartext)?;
        self.last_progress_ms = now_ms;

        match packet {
            Packet::Data(data) => {
                let Some(ring) = self
                    .recv
                    .get_mut(data.channel as usize)
                    .and_then(Option::as_mut)
                else {
                    return Ok(Inbound::Unhandled {
                        channel: data.channel,
                    });
                };
                // **Only an accepted store is acknowledged, and the
                // acknowledgement is its own.** The trigger names the fragment
                // that was stored, and the negative bit is this channel's gap
                // at that moment: a fragment more than two past the frontier
                // reveals a loss, anything nearer is reordering. A store the
                // ring refused names nothing -- the peer clears the slot for a
                // fragment it is told arrived, so naming one that was not kept
                // loses it for good.
                if ring.store(data.seq, data.body) == Stored::Accepted {
                    let nack = seq::gt(data.seq, ring.cumulative_ack().wrapping_add(2));
                    // A pending negative acknowledgement is not displaced by a
                    // later clean arrival: it rides the next acknowledgement
                    // out, where the peer's fast retransmission waits on it.
                    let held =
                        self.ack_due && self.ack_kind == AckKind::Ack && self.trigger_nack && !nack;
                    if !held {
                        self.trigger = (data.channel, data.seq);
                        self.trigger_nack = nack;
                    }
                    // **The cadence has two floors on one timestamp.** A gap
                    // or the last fragment of a message is answered at once;
                    // anything else waits out the data floor, and what it
                    // advanced rides the keepalive if no later arrival answers
                    // first. A held negative is already due.
                    if nack || data.last || now_ms - self.last_ack_sent_ms >= ACK_DATA_FLOOR_MS {
                        self.ack_due = true;
                        self.ack_kind = AckKind::Ack;
                    }
                }
                Ok(Inbound::Data {
                    channel: data.channel,
                })
            }
            Packet::Ack(ack) => {
                self.identify_peer(&ack);
                match ack.kind {
                    AckKind::Ack => {
                        let mut sample = None;
                        for ring in self.send.iter_mut().flatten() {
                            if let Some(taken) = ring.on_ack(&ack, now_ms) {
                                sample = Some(taken);
                            }
                        }
                        if let Some(sample) = sample {
                            self.observe_rtt(sample);
                        }
                        Ok(Inbound::Ack)
                    }
                    // A keepalive frees windows and proves liveness; its
                    // trigger is zeros, not a name, and must not reach the
                    // trigger path.
                    AckKind::Keepalive => {
                        for ring in self.send.iter_mut().flatten() {
                            ring.on_keepalive(&ack);
                        }
                        Ok(Inbound::Keepalive)
                    }
                }
            }
        }
    }

    /// Open every send ring's window once the peer's generation is known.
    ///
    /// **The peer's channel count is what a group acknowledgement reveals**:
    /// its entry count is the number of channels it carries, and only the
    /// current generation carries the full [`CHANNEL_COUNT`] and the deep
    /// ring that comes with it. A peer reporting fewer is the shallow
    /// generation, or is not yet distinguishable from it, so the window holds
    /// to the floor. Idempotent, and the send ring caps the raise at our own
    /// storage.
    fn identify_peer(&mut self, ack: &Ack) {
        if ack.reported < CHANNEL_COUNT {
            return;
        }
        if self.peer_identified {
            return;
        }
        self.peer_identified = true;
        let slots = u32::try_from(crate::channel::RING_SLOTS).unwrap_or(u32::MAX);
        for ring in self.send.iter_mut().flatten() {
            ring.raise_peer_depth(slots);
        }
    }

    /// Fold a round-trip sample into the smoothed estimate.
    ///
    /// The first sample seeds it outright; averaging against a zero start would
    /// leave the estimate an order of magnitude low for the first dozen
    /// samples, and the retransmission timeout is built on it.
    fn observe_rtt(&mut self, sample_ms: f64) {
        if !sample_ms.is_finite() || sample_ms < 0.0 {
            return;
        }
        if self.srtt_seeded {
            self.srtt_ms = self.srtt_ms * (1.0 - SRTT_ALPHA) + sample_ms * SRTT_ALPHA;
        } else {
            self.srtt_ms = sample_ms;
            self.srtt_seeded = true;
        }
    }

    /// Housekeeping. Safe to call whenever the loop wakes.
    pub fn poll(&mut self, now_ms: f64) {
        // Every acknowledgement resets the cadence, whatever prompted it, so
        // this fires only when nothing else has sent one. That is what makes it
        // a keepalive: the session is never silent for longer than the cadence,
        // and an idle one stays alive without a separate schedule.
        if !self.ack_due && now_ms - self.last_ack_sent_ms >= ACK_CADENCE_MS {
            self.ack_due = true;
            self.ack_kind = AckKind::Keepalive;
        }
        // **An empty channel is progress, not a stall.** A channel with
        // nothing outstanding can produce no acknowledgement, and a deadline
        // that did not say so would end every session that stopped sending.
        for (channel, ring) in self.send.iter().enumerate() {
            let (Some(ring), Some(entry)) = (ring.as_ref(), self.delivery.get_mut(channel)) else {
                continue;
            };
            let acked = ring.acked();
            if ring.in_flight() == 0 || acked != entry.acked_seen {
                entry.acked_seen = acked;
                entry.since_ms = now_ms;
            }
        }
    }

    /// Milliseconds until the session next needs attention.
    ///
    /// The shell arms its wait from this. There is no fixed tick: a loop that
    /// polls on a timer instead of on this will either burn cycles or miss
    /// deadlines, and both have shipped before.
    pub fn next_timer_ms(&self, now_ms: f64) -> f64 {
        let since_ack = now_ms - self.last_ack_sent_ms;
        (ACK_CADENCE_MS - since_ack).max(0.0)
    }

    /// Emit the next datagram, sealed and ready for the socket.
    ///
    /// Drive until `None`. Data is drained before acknowledgements, so a burst
    /// of media is not delayed behind bookkeeping.
    pub fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<usize>> {
        if !self.drain_started {
            for ring in self.send.iter_mut().flatten() {
                ring.begin_pass();
            }
            self.drain_channel = 0;
            self.drain_started = true;
        }

        // Cleartext is built directly at the ciphertext offset so sealing is a
        // header-and-tag write rather than a second pass over the payload.
        while self.drain_channel < CHANNEL_COUNT {
            let index = self.drain_channel;
            let srtt = self.srtt_ms;
            let level = self.level;
            let Some(ring) = self.send.get_mut(index).and_then(Option::as_mut) else {
                self.drain_channel += 1;
                continue;
            };
            let Some(body) = out.get_mut(ENVELOPE_LEN..) else {
                return Some(Err(Error::BufferTooSmall));
            };
            match ring.poll_send(now_ms, srtt, level, body) {
                Some(Ok(written)) => return Some(self.seal(written, out)),
                Some(Err(error)) => return Some(Err(error)),
                None => self.drain_channel += 1,
            }
        }

        if self.ack_due {
            self.ack_due = false;
            self.last_ack_sent_ms = now_ms;
            self.drain_started = false;
            return Some(self.emit_ack(out));
        }

        self.drain_started = false;
        None
    }

    /// Build and seal a group acknowledgement covering every channel.
    ///
    /// The trigger and the negative bit were captured at the accepted store
    /// they belong to. A keepalive carries the same nineteen cumulative counts
    /// but no trigger and no negative acknowledgement: nothing prompted it, so
    /// there is nothing to point at, and the flag combination with a trigger
    /// is not one a peer accepts.
    fn emit_ack(&mut self, out: &mut [u8]) -> Result<usize> {
        let mut cumulative = [0u32; CHANNEL_COUNT];
        for (index, slot) in self.recv.iter().enumerate() {
            let Some(ring) = slot.as_ref() else { continue };
            if let Some(entry) = cumulative.get_mut(index) {
                *entry = ring.cumulative_ack();
            }
        }
        let keepalive = self.ack_kind == AckKind::Keepalive;
        let ack = Ack {
            kind: self.ack_kind,
            nack: self.trigger_nack && !keepalive,
            trigger_channel: if keepalive { 0 } else { self.trigger.0 },
            trigger_seq: if keepalive { 0 } else { self.trigger.1 },
            cumulative,
            // We carry every channel, and [`packet::encode_ack`] writes them
            // all. A peer with fewer reads the prefix it understands.
            reported: CHANNEL_COUNT,
        };
        // The negative spans one emission: it was captured with the trigger
        // it belongs to, and the next acknowledgement carries its own.
        self.trigger_nack = false;
        let body = out.get_mut(ENVELOPE_LEN..).ok_or(Error::BufferTooSmall)?;
        let written = packet::encode_ack(body, &ack)?;
        self.seal(written, out)
    }

    /// Wrap cleartext already sitting at the ciphertext offset.
    fn seal(&mut self, cleartext_len: usize, out: &mut [u8]) -> Result<usize> {
        let counter = self.tx_counter;
        self.tx_counter = self.tx_counter.checked_add(1).ok_or(Error::Oversized)?;
        self.envelope.seal_in_place(counter, cleartext_len, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::SlotMeta;
    use crate::send::SendSlot;
    use std::vec::Vec;

    const SLOT: usize = 64;
    const SLOTS: usize = 32;
    const KEY: [u8; 32] = [3u8; 32];
    const VIDEO: u8 = 1;
    const CONTROL: u8 = 0;

    /// Storage for one endpoint: a receive and a send ring per channel.
    struct Arena {
        recv_bodies: Vec<u8>,
        recv_meta: Vec<SlotMeta>,
        send_bodies: Vec<u8>,
        send_meta: Vec<SendSlot>,
        control_recv_bodies: Vec<u8>,
        control_recv_meta: Vec<SlotMeta>,
        control_send_bodies: Vec<u8>,
        control_send_meta: Vec<SendSlot>,
    }

    impl Arena {
        fn new() -> Self {
            Self {
                recv_bodies: std::vec![0u8; SLOT * SLOTS],
                recv_meta: std::vec![SlotMeta::default(); SLOTS],
                send_bodies: std::vec![0u8; SLOT * SLOTS],
                send_meta: std::vec![SendSlot::default(); SLOTS],
                control_recv_bodies: std::vec![0u8; SLOT * SLOTS],
                control_recv_meta: std::vec![SlotMeta::default(); SLOTS],
                control_send_bodies: std::vec![0u8; SLOT * SLOTS],
                control_send_meta: std::vec![SendSlot::default(); SLOTS],
            }
        }
    }

    fn endpoint(arena: &mut Arena, now: f64) -> Session<'_> {
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, now);
        session
            .attach_recv(
                VIDEO,
                RecvRing::new(&mut arena.recv_bodies, &mut arena.recv_meta, SLOT).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                VIDEO,
                SendRing::new(&mut arena.send_bodies, &mut arena.send_meta, SLOT, VIDEO).unwrap(),
            )
            .unwrap();
        session
    }

    /// An endpoint carrying both channels, with the video receive ring
    /// optionally missing.
    ///
    /// **A peer that is not draining what it is sent** looks exactly like
    /// this from the far side: its transport still acknowledges the channel it
    /// is keeping up with, and the one it is not never advances.
    fn endpoint_pair(arena: &mut Arena, now: f64, video_recv: bool) -> Session<'_> {
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, now);
        if video_recv {
            session
                .attach_recv(
                    VIDEO,
                    RecvRing::new(&mut arena.recv_bodies, &mut arena.recv_meta, SLOT).unwrap(),
                )
                .unwrap();
        }
        session
            .attach_recv(
                CONTROL,
                RecvRing::new(
                    &mut arena.control_recv_bodies,
                    &mut arena.control_recv_meta,
                    SLOT,
                )
                .unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                VIDEO,
                SendRing::new(&mut arena.send_bodies, &mut arena.send_meta, SLOT, VIDEO).unwrap(),
            )
            .unwrap();
        session
            .attach_send(
                CONTROL,
                SendRing::new(
                    &mut arena.control_send_bodies,
                    &mut arena.control_send_meta,
                    SLOT,
                    CONTROL,
                )
                .unwrap(),
            )
            .unwrap();
        session
    }

    /// Drain one endpoint into nothing, which is a path that is not carrying.
    fn discard(from: &mut Session<'_>, now: f64) {
        let mut wire = [0u8; 512];
        while let Some(result) = from.get_output(now, &mut wire) {
            result.unwrap();
        }
    }

    impl Session<'_> {
        /// Test helper: combined window and stale counts across channels.
        fn pressure(&self) -> (u32, u32) {
            let mut window = 0u32;
            let mut stale = 0u32;
            for ring in self.send.iter().flatten() {
                window = window.saturating_add(ring.in_flight());
                stale = stale.saturating_add(ring.stale());
            }
            (window, stale)
        }
    }

    /// Drain one endpoint until it emits an acknowledgement, and return it.
    fn next_ack(session: &mut Session<'_>, now: f64) -> Ack {
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        loop {
            let written = session
                .get_output(now, &mut wire)
                .expect("nothing left to emit and no acknowledgement seen")
                .unwrap();
            let opened = session
                .envelope
                .open(&wire[..written], &mut scratch)
                .unwrap();
            if let Packet::Ack(ack) = packet::parse(opened.cleartext).unwrap() {
                return ack;
            }
        }
    }

    /// Drain one endpoint into the other, returning how many datagrams moved.
    fn pump(from: &mut Session<'_>, to: &mut Session<'_>, now: f64) -> usize {
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let mut moved = 0;
        while let Some(result) = from.get_output(now, &mut wire) {
            let written = result.unwrap();
            to.process_input(&wire[..written], now, &mut scratch)
                .unwrap();
            moved += 1;
        }
        moved
    }

    #[test]
    fn a_message_crosses_a_loopback_pair() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        left.send_message(VIDEO, b"hdr", b"payload").unwrap();
        assert!(pump(&mut left, &mut right, 1.0) >= 1);

        let mut out = [0u8; 256];
        let len = right.take_message(VIDEO, &mut out).unwrap().unwrap();
        assert_eq!(&out[..len], b"hdrpayload");
    }

    #[test]
    fn a_multi_fragment_message_crosses_intact() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        let payload: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
        left.send_message(VIDEO, &[], &payload).unwrap();
        pump(&mut left, &mut right, 1.0);

        let mut out = [0u8; 1024];
        let len = right.take_message(VIDEO, &mut out).unwrap().unwrap();
        assert_eq!(&out[..len], &payload[..]);
    }

    /// The acknowledgement path closes the loop: the sender's window must free
    /// once the receiver's acknowledgement comes back.
    #[test]
    fn acknowledgements_free_the_senders_window() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        left.send_message(VIDEO, &[], b"x").unwrap();
        pump(&mut left, &mut right, 1.0);
        let mut out = [0u8; 64];
        right.take_message(VIDEO, &mut out);

        // The receiver acknowledges on its next drain.
        right.poll(40.0);
        assert!(pump(&mut right, &mut left, 40.0) >= 1);

        // Nothing is outstanding now, so a second pass sends nothing.
        assert_eq!(
            pump(&mut left, &mut right, 41.0),
            0,
            "retransmitted an acknowledged fragment"
        );
    }

    #[test]
    fn a_round_trip_seeds_the_smoothed_estimate() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        left.send_message(VIDEO, &[], b"x").unwrap();
        pump(&mut left, &mut right, 10.0);
        right.poll(50.0);
        pump(&mut right, &mut left, 50.0);

        assert!(left.srtt_ms() > 0.0, "no sample was taken");
        assert!((left.srtt_ms() - 40.0).abs() < 1.0, "{}", left.srtt_ms());
    }

    #[test]
    fn the_nonce_counter_never_repeats() {
        let mut arena = Arena::new();
        let mut session = endpoint(&mut arena, 0.0);
        let mut wire = [0u8; 512];
        let mut seen = Vec::new();
        for round in 0..8 {
            session.send_message(VIDEO, &[], b"x").unwrap();
            while let Some(result) = session.get_output(f64::from(round), &mut wire) {
                let written = result.unwrap();
                seen.push(wire[3..11].to_vec());
                assert!(written > ENVELOPE_LEN);
            }
        }
        let unique: std::collections::BTreeSet<_> = seen.iter().collect();
        assert_eq!(unique.len(), seen.len(), "a nonce counter repeated");
    }

    /// An acknowledgement the cadence produced carries the keepalive flag and
    /// points at nothing, because nothing prompted it.
    #[test]
    fn a_cadence_acknowledgement_is_flagged_keepalive() {
        let mut arena = Arena::new();
        let mut session = endpoint(&mut arena, 0.0);
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];

        session.poll(ACK_CADENCE_MS);
        let written = session
            .get_output(ACK_CADENCE_MS, &mut wire)
            .unwrap()
            .unwrap();

        let opened = session
            .envelope
            .open(&wire[..written], &mut scratch)
            .unwrap();
        let Packet::Ack(ack) = packet::parse(opened.cleartext).unwrap() else {
            panic!("expected an acknowledgement");
        };
        assert_eq!(ack.kind, AckKind::Keepalive);
        assert!(!ack.nack);
        assert_eq!((ack.trigger_channel, ack.trigger_seq), (0, 0));
    }

    /// One that data prompted is an ordinary acknowledgement and does point at
    /// what prompted it, which is what drives the peer's fast retransmission.
    #[test]
    fn a_data_acknowledgement_keeps_its_trigger() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        left.send_message(VIDEO, &[], b"x").unwrap();
        pump(&mut left, &mut right, 1.0);

        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let written = right.get_output(2.0, &mut wire).unwrap().unwrap();
        let opened = right.envelope.open(&wire[..written], &mut scratch).unwrap();
        let Packet::Ack(ack) = packet::parse(opened.cleartext).unwrap() else {
            panic!("expected an acknowledgement");
        };
        assert_eq!(ack.kind, AckKind::Ack);
        assert_eq!(ack.trigger_channel, VIDEO);
    }

    /// **The data floor.** A fragment that ends no message and reveals no gap,
    /// arriving inside the floor, is not answered: an acknowledgement costs a
    /// datagram, and a video channel at full rate would otherwise produce a
    /// hundred of them a second. What it advanced rides the next
    /// acknowledgement of any kind, so nothing is lost by waiting.
    #[test]
    fn a_fragment_inside_the_data_floor_waits() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        let payload: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
        left.send_message(VIDEO, &[], &payload).unwrap();

        // The first fragment ends no message and lands inside the floor:
        // nothing is owed yet.
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let written = left.get_output(1.0, &mut wire).unwrap().unwrap();
        right
            .process_input(&wire[..written], 1.0, &mut scratch)
            .unwrap();
        let mut out = [0u8; 512];
        assert!(
            right.get_output(1.0, &mut out).is_none(),
            "a fragment inside the floor was answered"
        );

        // Once the floor has passed, an arrival of the same kind is answered.
        let written = left.get_output(11.0, &mut wire).unwrap().unwrap();
        right
            .process_input(&wire[..written], 11.0, &mut scratch)
            .unwrap();
        let ack = next_ack(&mut right, 11.0);
        assert_eq!(ack.kind, AckKind::Ack);
        assert!(!ack.nack);
    }

    /// The floor's first bypass: a gap is answered at once, because the peer's
    /// fast retransmission waits on it.
    #[test]
    fn a_negative_acknowledgement_does_not_wait_for_the_floor() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        // A delivered fragment, answered inside the floor because it ends its
        // message, so the floor is running when the loss lands.
        left.send_message(VIDEO, &[], b"a").unwrap();
        pump(&mut left, &mut right, 1.0);
        next_ack(&mut right, 1.0);

        // Sequences 1 to 3 are lost; a fragment of sequence 4 that ends no
        // message lands inside the floor. The gap alone answers it.
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let body = wire.get_mut(ENVELOPE_LEN..).unwrap();
        let written = packet::encode_data(
            body,
            &packet::Data {
                channel: VIDEO,
                seq: 4,
                last: false,
                body: b"x",
            },
        )
        .unwrap();
        let written = left.seal(written, &mut wire).unwrap();
        right
            .process_input(&wire[..written], 2.0, &mut scratch)
            .unwrap();

        let ack = next_ack(&mut right, 2.0);
        assert!(ack.nack, "a loss inside the floor waited");
        assert_eq!((ack.trigger_channel, ack.trigger_seq), (VIDEO, 4));
    }

    /// The floor's second bypass: the last fragment of a message is answered
    /// at once, which is what keeps the control handshake and small messages
    /// at full speed whatever the floor.
    #[test]
    fn a_message_tail_does_not_wait_for_the_floor() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        // Wide enough for several fragments, delivered whole and well inside
        // the floor: every fragment but the last is too early to answer, and
        // the last is answered because it ends the message.
        let payload: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
        let fragments = left.send_message(VIDEO, &[], &payload).unwrap();
        assert!(fragments > 2, "the message has to have a middle");
        pump(&mut left, &mut right, 1.0);

        let ack = next_ack(&mut right, 1.0);
        assert_eq!(ack.kind, AckKind::Ack);
        assert!(!ack.nack);
        assert_eq!(
            (ack.trigger_channel, ack.trigger_seq),
            (VIDEO, fragments - 1),
            "the acknowledgement did not name the tail"
        );
    }

    /// The regression for an idle session dying. Nothing is sent by the
    /// application for well past the hard liveness deadline, and both ends stay
    /// alive on the cadence alone.
    #[test]
    fn an_idle_pair_survives_past_the_hard_liveness_deadline() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        let mut now = 0.0;
        while now < LIVENESS_HARD_MS + 10_000.0 {
            now += ACK_CADENCE_MS;
            left.poll(now);
            right.poll(now);
            pump(&mut left, &mut right, now);
            pump(&mut right, &mut left, now);
        }

        assert_eq!(left.health(now), Health::Alive, "the idle sender died");
        assert_eq!(right.health(now), Health::Alive, "the idle receiver died");
    }

    /// **The regression for a peer that stopped receiving and never stopped
    /// talking.** Its acknowledgements keep arriving on the cadence, so every
    /// deadline that watches the inbound direction is satisfied for ever,
    /// while nothing queued for it is ever delivered and all of it is
    /// retransmitted for as long as the session lasts.
    #[test]
    fn a_peer_that_acknowledges_nothing_is_undeliverable() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        left.send_message(VIDEO, &[], b"x").unwrap();

        let mut now = 0.0;
        while now < DELIVERY_DEADLINE_MS {
            now += ACK_CADENCE_MS;
            left.poll(now);
            right.poll(now);
            // Only one direction. The peer is heard from throughout and
            // receives nothing, which is the shape of a broken return path and
            // of a peer that has stopped reading.
            discard(&mut left, now);
            pump(&mut right, &mut left, now);
        }

        assert_eq!(
            left.health(now),
            Health::Undeliverable,
            "a window that has moved by nothing for the whole deadline"
        );
        assert_eq!(
            right.health(now),
            Health::Alive,
            "the end with nothing outstanding is not the one at fault"
        );
    }

    /// **The peer is reachable, is acknowledging, and is still not receiving
    /// the stream.** Its control channel keeps up while its video ring takes
    /// nothing, which is what a peer that has stopped draining looks like from
    /// here. A deadline summed across channels never fires, because the cheap
    /// channel refreshes it for the expensive one.
    #[test]
    fn a_channel_that_is_not_draining_is_undeliverable_while_another_keeps_up() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint_pair(&mut left_arena, 0.0, true);
        // No video receive ring: everything sent on it is unhandled and never
        // acknowledged, while control is taken normally.
        let mut right = endpoint_pair(&mut right_arena, 0.0, false);

        left.send_message(VIDEO, &[], b"x").unwrap();

        let mut now = 0.0;
        while now < DELIVERY_DEADLINE_MS {
            now += ACK_CADENCE_MS;
            left.send_message(CONTROL, &[], b"c").unwrap();
            left.poll(now);
            right.poll(now);
            pump(&mut left, &mut right, now);
            pump(&mut right, &mut left, now);
            let mut body = [0u8; SLOT];
            while right.take_message(CONTROL, &mut body).is_some() {}
        }

        assert_eq!(
            left.health(now),
            Health::Undeliverable,
            "the channel that is not moving decides this, not the one that is"
        );
    }

    /// The control for the test above: the same duration, the same traffic,
    /// and the acknowledgements getting through. **A congested path fills a
    /// window and looks identical from the send side**, so the deadline has to
    /// be judged on the acknowledgements and not on the window.
    #[test]
    fn a_peer_that_keeps_acknowledging_survives_the_delivery_deadline() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        // **Something is outstanding the whole way through**, enqueued at the
        // end of each pass and acknowledged at the start of the next, so the
        // window is never empty when health is judged. A deadline that read
        // the window rather than the acknowledgements would end this session,
        // and this is the traffic it would end.
        let mut now = 0.0;
        left.send_message(VIDEO, &[], b"x").unwrap();
        while now < DELIVERY_DEADLINE_MS + 5_000.0 {
            now += ACK_CADENCE_MS;
            left.poll(now);
            right.poll(now);
            pump(&mut left, &mut right, now);
            pump(&mut right, &mut left, now);
            // Read, so the receive ring keeps taking new fragments.
            let mut body = [0u8; SLOT];
            while right.take_message(VIDEO, &mut body).is_some() {}
            left.send_message(VIDEO, &[], b"x").unwrap();
        }

        assert!(
            left.pressure().0 > 0,
            "the check is only worth anything with a window to misread"
        );
        assert_eq!(left.health(now), Health::Alive, "a delivering peer died");
    }

    #[test]
    fn liveness_degrades_then_dies() {
        let mut arena = Arena::new();
        let session = endpoint(&mut arena, 0.0);
        assert_eq!(session.health(0.0), Health::Alive);
        assert_eq!(session.health(LIVENESS_SOFT_MS), Health::Stalled);
        assert_eq!(session.health(LIVENESS_HARD_MS), Health::Dead);
    }

    #[test]
    fn the_timer_tracks_the_acknowledgement_cadence() {
        let mut arena = Arena::new();
        let session = endpoint(&mut arena, 0.0);
        assert!((session.next_timer_ms(0.0) - ACK_CADENCE_MS).abs() < 1e-9);
        assert!((session.next_timer_ms(10.0) - 20.0).abs() < 1e-9);
        assert!(
            (session.next_timer_ms(1000.0)).abs() < 1e-9,
            "must not go negative"
        );
    }

    /// The negative acknowledgement is the accepted store's own: its bit is
    /// the storing channel's gap at that moment, a reorder of two or less is
    /// not a gap, and a pending negative is not displaced by a later clean
    /// arrival on another channel.
    #[test]
    fn the_negative_acknowledgement_is_the_accepted_stores_own() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint_pair(&mut left_arena, 0.0, true);
        let mut right = endpoint_pair(&mut right_arena, 0.0, true);

        // In order: an ordinary acknowledgement naming what arrived.
        left.send_message(CONTROL, &[], b"a").unwrap();
        pump(&mut left, &mut right, 1.0);
        let ack = next_ack(&mut right, 1.0);
        assert!(!ack.nack);
        assert_eq!((ack.trigger_channel, ack.trigger_seq), (CONTROL, 0));

        // Sequence 1 is lost; 2 and 3 arrive. A gap of two is reordering and
        // fires no negative acknowledgement.
        left.send_message(CONTROL, &[], b"b").unwrap();
        discard(&mut left, 2.0);
        left.send_message(CONTROL, &[], b"c").unwrap();
        left.send_message(CONTROL, &[], b"d").unwrap();
        pump(&mut left, &mut right, 3.0);
        let ack = next_ack(&mut right, 3.0);
        assert!(
            !ack.nack,
            "a reorder of two fired a negative acknowledgement"
        );
        assert_eq!((ack.trigger_channel, ack.trigger_seq), (CONTROL, 3));

        // Sequence 4 is the third past the frontier: that is a loss.
        left.send_message(CONTROL, &[], b"e").unwrap();
        pump(&mut left, &mut right, 4.0);
        // A clean arrival on another channel before the acknowledgement
        // leaves must not displace the pending negative.
        left.send_message(VIDEO, &[], b"v").unwrap();
        pump(&mut left, &mut right, 5.0);
        let ack = next_ack(&mut right, 5.0);
        assert!(ack.nack, "the loss was not reported");
        assert_eq!(
            (ack.trigger_channel, ack.trigger_seq),
            (CONTROL, 4),
            "the negative acknowledgement was displaced"
        );

        // The negative spans one emission; the next acknowledgement is its
        // own again.
        left.send_message(VIDEO, &[], b"w").unwrap();
        pump(&mut left, &mut right, 6.0);
        let ack = next_ack(&mut right, 6.0);
        assert!(!ack.nack);
        assert_eq!((ack.trigger_channel, ack.trigger_seq), (VIDEO, 1));
    }

    /// A store the ring refused is never named: the peer clears the slot for
    /// a fragment it is told arrived, so naming one that was not kept loses
    /// it for good.
    #[test]
    fn a_refused_store_is_never_named_by_the_acknowledgement() {
        // A sender with a deeper ring than the receiver, which is what a peer
        // of a later generation looks like.
        let mut rogue_bodies = std::vec![0u8; SLOT * SLOTS * 2];
        let mut rogue_meta = std::vec![SendSlot::default(); SLOTS * 2];
        let mut left = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        left.attach_send(
            CONTROL,
            SendRing::new(&mut rogue_bodies, &mut rogue_meta, SLOT, CONTROL).unwrap(),
        )
        .unwrap();

        let mut right_arena = Arena::new();
        let mut right = endpoint_pair(&mut right_arena, 0.0, true);

        // More single-fragment messages than the receiver's ring holds.
        for _ in 0..SLOTS + 8 {
            left.send_message(CONTROL, &[], b"x").unwrap();
        }
        pump(&mut left, &mut right, 1.0);

        let drops = right.recv_drops(CONTROL).unwrap();
        assert_eq!(
            drops.out_of_window, 8,
            "the refused stores were not counted"
        );

        let ack = next_ack(&mut right, 1.0);
        assert_eq!(
            (ack.trigger_channel, ack.trigger_seq),
            (CONTROL, SLOTS as u32 - 1),
            "the acknowledgement named a fragment that was refused"
        );
        assert!(!ack.nack);
    }

    /// A fragment wider than a slot is refused, counted, and acknowledged by
    /// nothing at all.
    #[test]
    fn a_fragment_too_large_for_a_slot_produces_no_acknowledgement() {
        let mut rogue_bodies = std::vec![0u8; SLOT * 2 * SLOTS];
        let mut rogue_meta = std::vec![SendSlot::default(); SLOTS];
        let mut left = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        left.attach_send(
            CONTROL,
            SendRing::new(&mut rogue_bodies, &mut rogue_meta, SLOT * 2, CONTROL).unwrap(),
        )
        .unwrap();

        let mut right_arena = Arena::new();
        let mut right = endpoint_pair(&mut right_arena, 0.0, true);

        // One fragment of a hundred bytes against sixty-four byte slots.
        left.send_message(CONTROL, &[], &[7u8; 100]).unwrap();
        pump(&mut left, &mut right, 1.0);

        assert_eq!(right.recv_drops(CONTROL).unwrap().too_large, 1);
        let mut wire = [0u8; 512];
        assert!(
            right.get_output(2.0, &mut wire).is_none(),
            "a refused store produced an acknowledgement"
        );
    }

    /// A retransmission of what is already held is routine, is counted, and
    /// produces no acknowledgement of its own.
    #[test]
    fn a_duplicate_store_produces_no_acknowledgement() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint_pair(&mut left_arena, 0.0, true);
        let mut right = endpoint_pair(&mut right_arena, 0.0, true);

        left.send_message(CONTROL, &[], b"a").unwrap();
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let written = left.get_output(1.0, &mut wire).unwrap().unwrap();
        right
            .process_input(&wire[..written], 1.0, &mut scratch)
            .unwrap();
        next_ack(&mut right, 1.0);

        // The same datagram again, as a retransmission delivers it.
        right
            .process_input(&wire[..written], 2.0, &mut scratch)
            .unwrap();
        assert_eq!(right.recv_drops(CONTROL).unwrap().duplicate, 1);
        assert!(
            right.get_output(2.0, &mut wire).is_none(),
            "a duplicate produced an acknowledgement"
        );
    }

    /// **The send window holds to the shallow-ring floor until the peer
    /// reports the full channel count.** A peer that acknowledges with fewer
    /// channels is a generation whose ring may be as small as the floor, so
    /// running past it would wrap onto its occupied slots; the full count is
    /// what says the deep ring is safe.
    #[test]
    fn the_window_opens_when_the_peer_reports_every_channel() {
        use crate::channel::{PEER_RING_FLOOR, RING_SLOTS};
        use crate::packet::{Ack, AckKind};

        // A send ring deeper than the floor, so the floor is what bounds it
        // rather than our storage.
        const DEEP: usize = PEER_RING_FLOOR as usize + 200;
        let mut bodies = std::vec![0u8; SLOT * DEEP];
        let mut meta = std::vec![SendSlot::default(); DEEP];
        let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
        session
            .attach_send(
                VIDEO,
                SendRing::new(&mut bodies, &mut meta, SLOT, VIDEO).unwrap(),
            )
            .unwrap();

        assert_eq!(
            session
                .send
                .get(VIDEO as usize)
                .and_then(|r| r.as_ref())
                .map(SendRing::window_free),
            Some(PEER_RING_FLOOR as usize),
            "an unidentified peer's window is not the floor"
        );

        // A short acknowledgement -- a four-channel peer -- does not open it.
        let mut short = [0u32; CHANNEL_COUNT];
        short[VIDEO as usize] = 0;
        session.identify_peer(&Ack {
            reported: 4,
            kind: AckKind::Keepalive,
            nack: false,
            trigger_channel: 0,
            trigger_seq: 0,
            cumulative: short,
        });
        assert_eq!(
            session
                .send
                .get(VIDEO as usize)
                .and_then(|r| r.as_ref())
                .map(SendRing::window_free),
            Some(PEER_RING_FLOOR as usize),
            "a short acknowledgement opened the window"
        );

        // The full channel count identifies the current generation.
        session.identify_peer(&Ack {
            reported: CHANNEL_COUNT,
            kind: AckKind::Keepalive,
            nack: false,
            trigger_channel: 0,
            trigger_seq: 0,
            cumulative: [0u32; CHANNEL_COUNT],
        });
        assert_eq!(
            session
                .send
                .get(VIDEO as usize)
                .and_then(|r| r.as_ref())
                .map(SendRing::window_free),
            Some(DEEP.min(RING_SLOTS)),
            "the window did not open to the deep ring"
        );
    }

    /// The regression for a keepalive read as an acknowledgement of channel 0,
    /// sequence 0. A keepalive points at nothing, and the zeros in its trigger
    /// are not a name: reading them as one clears the control channel's first
    /// fragment while it is in flight and reports its age as a round trip, so
    /// a lost first fragment is never retransmitted and the channel wedges
    /// until the delivery deadline.
    #[test]
    fn a_keepalive_takes_no_trigger_slot_and_no_round_trip_sample() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint_pair(&mut left_arena, 0.0, true);
        let mut right = endpoint_pair(&mut right_arena, 0.0, true);

        // The first control fragment goes out and is lost.
        left.send_message(CONTROL, &[], b"x").unwrap();
        discard(&mut left, 0.0);

        // The peer heard nothing, so its cadence produces a keepalive.
        right.poll(ACK_CADENCE_MS);
        assert!(pump(&mut right, &mut left, ACK_CADENCE_MS) >= 1);

        assert!(
            left.srtt_ms().abs() < 1e-9,
            "a keepalive produced a round-trip sample of {}",
            left.srtt_ms()
        );

        // Well past the retransmission timeout the fragment must go out again.
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let mut resent = false;
        while let Some(result) = left.get_output(200.0, &mut wire) {
            let written = result.unwrap();
            let opened = left.envelope.open(&wire[..written], &mut scratch).unwrap();
            if let Packet::Data(data) = packet::parse(opened.cleartext).unwrap() {
                assert_eq!((data.channel, data.seq), (CONTROL, 0));
                resent = true;
            }
        }
        assert!(resent, "the lost fragment was never retransmitted");
    }

    /// A keepalive still frees the window: its cumulative counts are as good
    /// as any acknowledgement's.
    #[test]
    fn a_keepalive_still_frees_the_window() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint_pair(&mut left_arena, 0.0, true);
        let mut right = endpoint_pair(&mut right_arena, 0.0, true);

        left.send_message(CONTROL, &[], b"x").unwrap();
        pump(&mut left, &mut right, 1.0);
        let mut body = [0u8; SLOT];
        right.take_message(CONTROL, &mut body);

        // The ordinary acknowledgement is emitted and lost, so the only word
        // that reaches the sender is the keepalive the cadence produces next.
        discard(&mut right, 2.0);
        right.poll(ACK_CADENCE_MS + 2.0);
        pump(&mut right, &mut left, ACK_CADENCE_MS + 2.0);

        let (window, _) = left.pressure();
        assert_eq!(window, 0, "the keepalive did not free the window");
    }

    #[test]
    fn a_channel_without_a_ring_is_reported_not_fatal() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        // Left sends on the video channel; right detaches its ring first.
        right.recv[VIDEO as usize] = None;
        left.send_message(VIDEO, &[], b"x").unwrap();

        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let result = left.get_output(1.0, &mut wire).unwrap().unwrap();
        assert_eq!(
            right
                .process_input(&wire[..result], 1.0, &mut scratch)
                .unwrap(),
            Inbound::Unhandled { channel: VIDEO }
        );
    }

    #[test]
    fn a_forged_datagram_is_refused() {
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = endpoint(&mut left_arena, 0.0);
        let mut right = endpoint(&mut right_arena, 0.0);

        left.send_message(VIDEO, &[], b"x").unwrap();
        let mut wire = [0u8; 512];
        let mut scratch = [0u8; 512];
        let written = left.get_output(1.0, &mut wire).unwrap().unwrap();
        wire[ENVELOPE_LEN] ^= 0xFF;
        assert_eq!(
            right.process_input(&wire[..written], 1.0, &mut scratch),
            Err(Error::Decrypt)
        );
    }
}
