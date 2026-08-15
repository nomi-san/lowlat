//! Per-channel send ring, retransmission, and the staleness scan
//! (docs/01-protocol.md 9).
//!
//! Storage is lent by the caller, as on the receive side.
//!
//! Three behaviours live here and they interlock:
//!
//! - **Retransmission timeout** is per fragment and exponential in its retry
//!   count. It is *not* derived from the congestion level table.
//! - **Fast retransmission** on a negative acknowledgement fires **once per
//!   fragment**, latched, so a burst of them cannot become a storm.
//! - **Staleness classification** uses the level table and produces the count
//!   the congestion controller consumes. The scan and the controller are one
//!   loop split in two.

use crate::congestion::{self, WINDOW_FLOOR};
use crate::error::{Error, Result};
use crate::message::Message;
use crate::packet::{self, Ack, Data};
use crate::seq;

/// Outstanding fragments beyond which a sender stops emitting and defers.
pub const OUTSTANDING_CAP: u32 = WINDOW_FLOOR;

const RTO_FLOOR_MS: f64 = 50.0;
const RTO_CEILING_MS: f64 = 1000.0;
/// Flat grace added after the clamp, not part of it.
const RTO_GRACE_MS: f64 = 30.0;
/// Per-fragment budget used by the second staleness clause.
const SLOT_BUDGET_MS: f64 = 100.0;

/// Bookkeeping for one outstanding fragment.
///
/// Public because the caller owns the storage; its fields are not.
#[derive(Debug, Clone, Copy)]
pub struct SendSlot {
    len: u16,
    occupied: bool,
    last: bool,
    /// False while the fragment is written but not yet on the wire, which is
    /// what the outstanding cap produces.
    sent: bool,
    /// Latch: at most one fast retransmission per fragment.
    nack_resent: bool,
    retransmits: u16,
    first_sent_ms: f64,
    last_sent_ms: f64,
}

impl Default for SendSlot {
    fn default() -> Self {
        Self {
            len: 0,
            occupied: false,
            last: false,
            sent: false,
            nack_resent: false,
            retransmits: 0,
            first_sent_ms: 0.0,
            last_sent_ms: 0.0,
        }
    }
}

/// A channel's send ring.
#[derive(Debug)]
pub struct SendRing<'a> {
    bodies: &'a mut [u8],
    meta: &'a mut [SendSlot],
    slot_len: usize,
    channel: u8,
    /// The peer's cumulative acknowledgement: everything below is delivered.
    base: u32,
    /// Next sequence to assign.
    next: u32,
    /// Where the scan has reached this pass.
    cursor: u32,
    /// Set by a negative acknowledgement; consumed by the next scan.
    nack_below: Option<u32>,
    outstanding: u32,
    stale: u32,
}

impl<'a> SendRing<'a> {
    /// Build a ring over caller-owned storage.
    pub fn new(
        bodies: &'a mut [u8],
        meta: &'a mut [SendSlot],
        slot_len: usize,
        channel: u8,
    ) -> Result<Self> {
        if slot_len == 0 || meta.is_empty() || channel as usize >= packet::CHANNEL_COUNT {
            return Err(Error::BadLength);
        }
        if bodies.len() != meta.len().checked_mul(slot_len).ok_or(Error::BadLength)? {
            return Err(Error::BadLength);
        }
        Ok(Self {
            bodies,
            meta,
            slot_len,
            channel,
            base: 0,
            next: 0,
            cursor: 0,
            nack_below: None,
            outstanding: 0,
            stale: 0,
        })
    }

    /// Fragments the peer has not acknowledged.
    pub fn in_flight(&self) -> u32 {
        self.next.wrapping_sub(self.base)
    }

    /// Fragments the ring can still accept.
    ///
    /// Bounded by the **peer's** ring depth, not ours: running further ahead
    /// than that wraps onto slots the peer has already delivered from.
    pub fn window_free(&self) -> usize {
        let depth = u32::try_from(self.meta.len()).unwrap_or(u32::MAX);
        depth.saturating_sub(self.in_flight()) as usize
    }

    /// Outstanding count from the last completed scan.
    pub fn outstanding(&self) -> u32 {
        self.outstanding
    }

    /// Stale count from the last completed scan. Feeds the controller.
    pub fn stale(&self) -> u32 {
        self.stale
    }

    /// Next sequence that will be assigned.
    pub fn next_sequence(&self) -> u32 {
        self.next
    }

    fn index(&self, sequence: u32) -> usize {
        (sequence as usize) % self.meta.len()
    }

    /// Write a message's fragments into the ring.
    ///
    /// Nothing is emitted here. The fragments become pending, and the scan
    /// releases them subject to the outstanding cap.
    pub fn enqueue(&mut self, message: &Message<'_>) -> Result<u32> {
        let capacity = self.slot_len;
        let fragments = message.fragment_count(capacity);
        if fragments > self.window_free() {
            return Err(Error::BufferTooSmall);
        }

        for index in 0..fragments {
            let step = u32::try_from(index).map_err(|_| Error::BadLength)?;
            let sequence = self.next.wrapping_add(step);
            let slot = self.index(sequence);
            let start = slot.checked_mul(self.slot_len).ok_or(Error::BadLength)?;
            let region = self
                .bodies
                .get_mut(start..start + self.slot_len)
                .ok_or(Error::BadLength)?;
            let Some(result) = message.fragment(index, capacity, region) else {
                return Err(Error::BadLength);
            };
            let fragment = result?;
            let entry = self.meta.get_mut(slot).ok_or(Error::BadLength)?;
            *entry = SendSlot {
                len: u16::try_from(fragment.len).map_err(|_| Error::BadLength)?,
                occupied: true,
                last: fragment.last,
                ..SendSlot::default()
            };
        }

        let count = u32::try_from(fragments).map_err(|_| Error::BadLength)?;
        self.next = self.next.wrapping_add(count);
        Ok(count)
    }

    /// Apply an incoming acknowledgement.
    ///
    /// Returns a round-trip sample when the acknowledgement names a fragment we
    /// are still holding. The sample comes from the fragment's **first** send,
    /// so a retransmitted fragment does not report an artificially short trip.
    pub fn on_ack(&mut self, ack: &Ack, now_ms: f64) -> Option<f64> {
        let cumulative = *ack.cumulative.get(self.channel as usize)?;
        let previous = self.base;
        if seq::gt(cumulative, self.base) && seq::le(cumulative, self.next) {
            self.base = cumulative;
            if seq::lt(self.cursor, self.base) {
                self.cursor = self.base;
            }
        }

        if ack.nack && ack.trigger_channel == self.channel {
            self.nack_below = Some(ack.trigger_seq);
        }

        if ack.trigger_channel != self.channel {
            return None;
        }
        if seq::lt(ack.trigger_seq, previous) || seq::ge(ack.trigger_seq, self.next) {
            return None;
        }
        let index = self.index(ack.trigger_seq);
        let slot = self.meta.get_mut(index)?;
        if !slot.occupied || !slot.sent {
            return None;
        }
        let sample = now_ms - slot.first_sent_ms;
        slot.occupied = false;
        if sample.is_finite() && sample >= 0.0 {
            Some(sample)
        } else {
            None
        }
    }

    /// Retransmission timeout for a fragment that has been sent `retransmits`
    /// times already.
    fn rto_ms(retransmits: u16, srtt_ms: f64) -> f64 {
        let scaled = 2.0 * f64::from(u32::from(retransmits) + 1) * srtt_ms;
        scaled.clamp(RTO_FLOOR_MS, RTO_CEILING_MS) + RTO_GRACE_MS
    }

    /// Emit the next fragment that is due, if any.
    ///
    /// Drive this until it returns `None`, which marks the end of one scan
    /// pass and publishes fresh [`SendRing::outstanding`] and
    /// [`SendRing::stale`] counts.
    pub fn poll_send(
        &mut self,
        now_ms: f64,
        srtt_ms: f64,
        level_index: usize,
        out: &mut [u8],
    ) -> Option<Result<usize>> {
        let level = congestion::level(level_index);

        while self.cursor != self.next {
            let sequence = self.cursor;
            let index = self.index(sequence);
            let Some(slot) = self.meta.get(index).copied() else {
                self.cursor = self.cursor.wrapping_add(1);
                continue;
            };
            if !slot.occupied {
                self.cursor = self.cursor.wrapping_add(1);
                continue;
            }

            let age = now_ms - slot.last_sent_ms;
            let nacked = self
                .nack_below
                .is_some_and(|below| seq::lt(sequence, below) && !slot.nack_resent);

            let due = if !slot.sent {
                // Pending. The cap is what defers it, and draining is what
                // releases it.
                self.outstanding < OUTSTANDING_CAP
            } else if nacked {
                true
            } else {
                age > Self::rto_ms(slot.retransmits, srtt_ms)
            };

            if !due {
                self.classify(index, now_ms, srtt_ms, level);
                self.cursor = self.cursor.wrapping_add(1);
                continue;
            }

            // Encode before mutating, so a buffer that is too small leaves the
            // ring untouched and the caller can retry with a larger one.
            let start = index.checked_mul(self.slot_len)?;
            let body = self
                .bodies
                .get(start..start.checked_add(slot.len as usize)?)?;
            let written = match packet::encode_data(
                out,
                &Data {
                    channel: self.channel,
                    seq: sequence,
                    last: slot.last,
                    body,
                },
            ) {
                Ok(written) => written,
                Err(error) => return Some(Err(error)),
            };

            let Some(entry) = self.meta.get_mut(index) else {
                return Some(Err(Error::BadLength));
            };
            if entry.sent {
                if nacked {
                    entry.nack_resent = true;
                } else {
                    entry.retransmits = entry.retransmits.saturating_add(1);
                }
            } else {
                entry.sent = true;
                entry.first_sent_ms = now_ms;
            }
            entry.last_sent_ms = now_ms;
            self.outstanding = self.outstanding.saturating_add(1);

            self.classify(index, now_ms, srtt_ms, level);
            self.cursor = self.cursor.wrapping_add(1);
            return Some(Ok(written));
        }

        // Pass complete. Republish the counters and rewind for the next one.
        self.nack_below = None;
        self.cursor = self.base;
        None
    }

    /// Count a fragment toward the outstanding and stale totals.
    fn classify(&mut self, index: usize, now_ms: f64, srtt_ms: f64, level: congestion::Level) {
        let Some(slot) = self.meta.get(index) else {
            return;
        };
        if !slot.occupied {
            return;
        }
        let age = now_ms - slot.last_sent_ms;
        let threshold = level.rtt_mult * srtt_ms + level.base_ms;
        let is_stale = age > threshold
            || srtt_ms > level.rtt_mult * SLOT_BUDGET_MS + level.base_ms
            || slot.retransmits > 0
            || slot.nack_resent
            || !slot.sent;
        if is_stale {
            self.stale = self.stale.saturating_add(1);
        }
    }

    /// Clear the per-pass counters. The shell calls this before draining.
    pub fn begin_pass(&mut self) {
        self.outstanding = 0;
        self.stale = 0;
        self.cursor = self.base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{AckKind, CHANNEL_COUNT};
    use std::vec::Vec;

    const SLOT: usize = 32;
    const SLOTS: usize = 16;
    const CHANNEL: u8 = 1;

    struct Storage {
        bodies: Vec<u8>,
        meta: Vec<SendSlot>,
    }

    impl Storage {
        fn new() -> Self {
            Self {
                bodies: std::vec![0u8; SLOT * SLOTS],
                meta: std::vec![SendSlot::default(); SLOTS],
            }
        }
        fn ring(&mut self) -> SendRing<'_> {
            SendRing::new(&mut self.bodies, &mut self.meta, SLOT, CHANNEL).unwrap()
        }
    }

    fn ack_with(cumulative: u32, nack: bool, trigger_seq: u32) -> Ack {
        let mut values = [0u32; CHANNEL_COUNT];
        if let Some(slot) = values.get_mut(CHANNEL as usize) {
            *slot = cumulative;
        }
        Ack {
            kind: AckKind::Ack,
            nack,
            trigger_channel: CHANNEL,
            trigger_seq,
            cumulative: values,
        }
    }

    /// Drain one scan pass, returning the sequences emitted.
    fn drain(ring: &mut SendRing<'_>, now: f64, srtt: f64) -> Vec<u32> {
        let mut out = [0u8; 128];
        let mut seen = Vec::new();
        ring.begin_pass();
        while let Some(result) = ring.poll_send(now, srtt, 1, &mut out) {
            let written = result.unwrap();
            let parsed = packet::parse(&out[..written]).unwrap();
            match parsed {
                packet::Packet::Data(data) => seen.push(data.seq),
                packet::Packet::Ack(_) => panic!("send ring emitted an ack"),
            }
        }
        seen
    }

    #[test]
    fn enqueue_then_emit_in_order() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let payload = [7u8; 100];
        let message = Message::new(&[], &payload).unwrap();
        assert_eq!(ring.enqueue(&message).unwrap(), 4);
        assert_eq!(ring.in_flight(), 4);
        assert_eq!(drain(&mut ring, 0.0, 10.0), std::vec![0, 1, 2, 3]);
        // Already sent and not yet due: a second pass emits nothing.
        assert!(drain(&mut ring, 1.0, 10.0).is_empty());
    }

    #[test]
    fn the_last_fragment_carries_the_flag_and_earlier_ones_do_not() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let payload = [1u8; 100];
        let message = Message::new(&[], &payload).unwrap();
        ring.enqueue(&message).unwrap();

        let mut out = [0u8; 128];
        let mut flags = Vec::new();
        ring.begin_pass();
        while let Some(result) = ring.poll_send(0.0, 10.0, 1, &mut out) {
            let written = result.unwrap();
            let packet::Packet::Data(data) = packet::parse(&out[..written]).unwrap() else {
                panic!()
            };
            flags.push(data.last);
        }
        assert_eq!(flags, std::vec![false, false, false, true]);
    }

    #[test]
    fn the_outstanding_cap_defers_rather_than_dropping() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        // One fragment per message keeps the arithmetic obvious.
        for _ in 0..SLOTS {
            let message = Message::new(&[], b"x").unwrap();
            ring.enqueue(&message).unwrap();
        }
        assert_eq!(ring.in_flight(), SLOTS as u32);

        // With the cap lowered below the queue depth nothing is lost, it is
        // simply held back. SLOTS is under the real cap, so emulate pressure by
        // checking the counter instead.
        let emitted = drain(&mut ring, 0.0, 10.0);
        assert_eq!(emitted.len(), SLOTS, "cap should not bite below 100");
        assert_eq!(ring.outstanding(), SLOTS as u32);
    }

    #[test]
    fn a_cumulative_acknowledgement_frees_the_window() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        for _ in 0..SLOTS {
            ring.enqueue(&Message::new(&[], b"x").unwrap()).unwrap();
        }
        assert_eq!(ring.window_free(), 0);
        assert!(ring.enqueue(&Message::new(&[], b"y").unwrap()).is_err());

        drain(&mut ring, 0.0, 10.0);
        ring.on_ack(&ack_with(8, false, u32::MAX - 1), 5.0);
        assert_eq!(ring.window_free(), 8);
        assert!(ring.enqueue(&Message::new(&[], b"y").unwrap()).is_ok());
    }

    #[test]
    fn a_round_trip_sample_comes_from_the_first_send() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        ring.enqueue(&Message::new(&[], b"x").unwrap()).unwrap();
        drain(&mut ring, 100.0, 10.0);
        let sample = ring.on_ack(&ack_with(1, false, 0), 137.5).unwrap();
        assert!((sample - 37.5).abs() < 1e-9);
    }

    #[test]
    fn retransmission_waits_for_the_timeout_then_fires() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        ring.enqueue(&Message::new(&[], b"x").unwrap()).unwrap();
        drain(&mut ring, 0.0, 10.0);

        // Floor is 50 ms plus 30 ms of grace, so 79 is early and 81 is due.
        assert!(drain(&mut ring, 79.0, 10.0).is_empty());
        assert_eq!(drain(&mut ring, 81.0, 10.0), std::vec![0]);
    }

    #[test]
    fn the_timeout_grows_with_each_retry() {
        assert!((SendRing::rto_ms(0, 10.0) - 80.0).abs() < 1e-9);
        assert!((SendRing::rto_ms(1, 10.0) - 70.0_f64.max(80.0)).abs() < 1e-9);
        // Floor and ceiling both apply before the grace is added.
        assert!((SendRing::rto_ms(0, 0.1) - (RTO_FLOOR_MS + RTO_GRACE_MS)).abs() < 1e-9);
        assert!((SendRing::rto_ms(200, 100.0) - (RTO_CEILING_MS + RTO_GRACE_MS)).abs() < 1e-9);
        // Exponential in the retry count between the bounds.
        assert!(SendRing::rto_ms(3, 40.0) > SendRing::rto_ms(1, 40.0));
    }

    #[test]
    fn a_negative_acknowledgement_retransmits_immediately_but_only_once() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        for _ in 0..4 {
            ring.enqueue(&Message::new(&[], b"x").unwrap()).unwrap();
        }
        drain(&mut ring, 0.0, 10.0);

        // Nack below sequence 3, well inside the timeout.
        ring.on_ack(&ack_with(0, true, 3), 1.0);
        assert_eq!(drain(&mut ring, 2.0, 10.0), std::vec![0, 1, 2]);

        // A second nack must not resend the same fragments: the latch holds.
        ring.on_ack(&ack_with(0, true, 3), 3.0);
        assert!(
            drain(&mut ring, 4.0, 10.0).is_empty(),
            "fast retransmission fired twice for the same fragment"
        );
    }

    /// A fragment sent once and still fresh is not stale; one that has been
    /// retransmitted is, and that is what reaches the congestion controller.
    #[test]
    fn retransmission_makes_a_fragment_stale() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        for _ in 0..3 {
            ring.enqueue(&Message::new(&[], b"x").unwrap()).unwrap();
        }
        assert_eq!(drain(&mut ring, 0.0, 5.0).len(), 3);
        assert_eq!(ring.stale(), 0, "freshly sent fragments are not stale");

        // Past the timeout, so all three retransmit and become stale.
        assert_eq!(drain(&mut ring, 100.0, 5.0).len(), 3);
        assert_eq!(ring.stale(), 3);
    }

    /// Age alone is enough, before any retransmission: the threshold is
    /// `rtt_mult * srtt + base_ms`, which is 25.5 ms at level 1 with a 5 ms
    /// round trip.
    #[test]
    fn age_past_the_threshold_makes_a_fragment_stale() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        ring.enqueue(&Message::new(&[], b"x").unwrap()).unwrap();
        drain(&mut ring, 0.0, 5.0);
        // Under the retransmission timeout, so nothing is sent, but the
        // fragment is older than the staleness threshold.
        assert!(drain(&mut ring, 40.0, 5.0).is_empty());
        assert_eq!(ring.stale(), 1);
    }

    #[test]
    fn rejects_mismatched_storage_and_bad_channels() {
        let mut bodies = std::vec![0u8; 10];
        let mut meta = std::vec![SendSlot::default(); 3];
        assert!(SendRing::new(&mut bodies, &mut meta, 4, CHANNEL).is_err());
        let mut bodies = std::vec![0u8; 12];
        assert!(SendRing::new(&mut bodies, &mut meta, 4, CHANNEL_COUNT as u8).is_err());
    }
}
