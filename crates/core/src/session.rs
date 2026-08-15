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

use crate::channel::RecvRing;
use crate::congestion::Controller;
use crate::envelope::{ENVELOPE_LEN, Envelope};
use crate::error::{Error, Result};
use crate::message::Message;
use crate::packet::{self, Ack, AckKind, CHANNEL_COUNT, Packet};
use crate::send::SendRing;

/// Longest gap between group acknowledgements while a session is alive.
pub const ACK_CADENCE_MS: f64 = 30.0;
/// No progress for this long is a soft failure.
pub const LIVENESS_SOFT_MS: f64 = 60_000.0;
/// No progress for this long is a hard failure.
pub const LIVENESS_HARD_MS: f64 = 120_000.0;

/// Weight given to a new round-trip sample.
const SRTT_ALPHA: f64 = 0.1;

/// What a datagram turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Inbound {
    /// A fragment was stored on this channel.
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
}

/// One peer-to-peer session.
#[derive(Debug)]
pub struct Session<'a> {
    envelope: Envelope,
    recv: [Option<RecvRing<'a>>; CHANNEL_COUNT],
    send: [Option<SendRing<'a>>; CHANNEL_COUNT],
    controller: Controller,
    level: usize,

    /// Monotonic per sender. Never reused: a wrap would repeat a nonce.
    tx_counter: u64,
    srtt_ms: f64,
    srtt_seeded: bool,

    last_ack_sent_ms: f64,
    last_progress_ms: f64,
    ack_due: bool,
    trigger: (u8, u32),

    /// Which channel the output drain is working through.
    drain_channel: usize,
    drain_started: bool,
}

impl<'a> Session<'a> {
    /// Build a session. Rings are attached separately, per channel.
    pub fn new(envelope: Envelope, level: usize, now_ms: f64) -> Self {
        Self {
            envelope,
            recv: core::array::from_fn(|_| None),
            send: core::array::from_fn(|_| None),
            controller: Controller::new(level, 1.0, 500.0),
            level,
            tx_counter: 0,
            srtt_ms: 0.0,
            srtt_seeded: false,
            last_ack_sent_ms: now_ms,
            last_progress_ms: now_ms,
            ack_due: false,
            trigger: (0, 0),
            drain_channel: 0,
            drain_started: false,
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

    /// Encoder rate the controller currently wants.
    pub fn rate_mbps(&self) -> f64 {
        self.controller.rate_mbps()
    }

    /// Liveness, judged against the last forward progress.
    pub fn health(&self, now_ms: f64) -> Health {
        let idle = now_ms - self.last_progress_ms;
        if idle >= LIVENESS_HARD_MS {
            Health::Dead
        } else if idle >= LIVENESS_SOFT_MS {
            Health::Stalled
        } else {
            Health::Alive
        }
    }

    /// Contiguous frontier on `channel`: what we would acknowledge.
    pub fn recv_cumulative(&self, channel: u8) -> Option<u32> {
        Some(self.recv.get(channel as usize)?.as_ref()?.cumulative_ack())
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
                self.trigger = (data.channel, data.seq);
                // Any data arrival makes an acknowledgement due; the cadence
                // check in get_output decides when it actually leaves.
                self.ack_due = true;
                let Some(ring) = self
                    .recv
                    .get_mut(data.channel as usize)
                    .and_then(Option::as_mut)
                else {
                    return Ok(Inbound::Unhandled {
                        channel: data.channel,
                    });
                };
                ring.store(data.seq, data.body);
                Ok(Inbound::Data {
                    channel: data.channel,
                })
            }
            Packet::Ack(ack) => {
                let mut sample = None;
                for ring in self.send.iter_mut().flatten() {
                    if let Some(taken) = ring.on_ack(&ack, now_ms) {
                        sample = Some(taken);
                    }
                }
                if let Some(sample) = sample {
                    self.observe_rtt(sample);
                }
                Ok(match ack.kind {
                    AckKind::Ack => Inbound::Ack,
                    AckKind::Keepalive => Inbound::Keepalive,
                })
            }
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
        if now_ms - self.last_ack_sent_ms >= ACK_CADENCE_MS {
            self.ack_due = true;
        }
        let (window, stale) = self.pressure();
        self.controller.tick(window, stale, 0.0);
    }

    /// Combined window and stale counts across channels, for the controller.
    fn pressure(&self) -> (u32, u32) {
        let mut window = 0u32;
        let mut stale = 0u32;
        for ring in self.send.iter().flatten() {
            window = window.saturating_add(ring.in_flight());
            stale = stale.saturating_add(ring.stale());
        }
        (window, stale)
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
    fn emit_ack(&mut self, out: &mut [u8]) -> Result<usize> {
        let mut cumulative = [0u32; CHANNEL_COUNT];
        let mut gap = false;
        for (index, slot) in self.recv.iter().enumerate() {
            let Some(ring) = slot.as_ref() else { continue };
            if let Some(entry) = cumulative.get_mut(index) {
                *entry = ring.cumulative_ack();
            }
            if ring.has_gap() {
                gap = true;
            }
        }
        let ack = Ack {
            kind: AckKind::Ack,
            nack: gap,
            trigger_channel: self.trigger.0,
            trigger_seq: self.trigger.1,
            cumulative,
        };
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

    /// Storage for one endpoint: a receive and a send ring on one channel.
    struct Arena {
        recv_bodies: Vec<u8>,
        recv_meta: Vec<SlotMeta>,
        send_bodies: Vec<u8>,
        send_meta: Vec<SendSlot>,
    }

    impl Arena {
        fn new() -> Self {
            Self {
                recv_bodies: std::vec![0u8; SLOT * SLOTS],
                recv_meta: std::vec![SlotMeta::default(); SLOTS],
                send_bodies: std::vec![0u8; SLOT * SLOTS],
                send_meta: std::vec![SendSlot::default(); SLOTS],
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
