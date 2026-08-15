//! Per-channel receive ring and message reassembly (docs/01-protocol.md 7).
//!
//! The ring is direct-mapped: a sequence number's slot is `seq mod slots`, so
//! it is a sliding window rather than a queue. **Peer ring depth is 4000 and is
//! a protocol constant**, not a tuning knob: a sender more than that far ahead
//! of the receiver's cumulative acknowledgement wraps onto occupied slots and
//! destroys data that was already delivered.
//!
//! Storage is supplied by the caller. This crate has no allocator, and a ring
//! sized for the protocol is megabytes, so the shell allocates once at session
//! setup and lends the memory here for the session's life.
//!
//! Two cursors, and conflating them is a bug:
//!
//! - `cumulative` is the contiguous frontier and is what we acknowledge.
//! - `delivered` is where the reader has consumed to. A slot may not be reused
//!   until `delivered` passes it, so the write window is bounded by `delivered`
//!   and never by `cumulative`.

use crate::error::{Error, Result};
use crate::message::{self, LENGTH_PREFIX_LEN};
use crate::seq;

/// Slots per channel per direction, fixed by the protocol.
pub const RING_SLOTS: usize = 4000;

/// Outcome of offering a fragment to the ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// Placed in its slot.
    Accepted,
    /// Already delivered, or already present. A retransmission, which is
    /// routine and not an error.
    Duplicate,
    /// Beyond the window. The peer has run further ahead than the ring can
    /// hold, which means data would be lost either way.
    OutOfWindow,
    /// Larger than a slot.
    TooLarge,
}

/// Per-slot bookkeeping, kept out of the body storage so bodies stay a flat
/// byte arena.
#[derive(Debug, Clone, Copy, Default)]
pub struct SlotMeta {
    len: u16,
    ready: bool,
}

/// A channel's receive ring.
#[derive(Debug)]
pub struct RecvRing<'a> {
    bodies: &'a mut [u8],
    meta: &'a mut [SlotMeta],
    slot_len: usize,
    /// Next sequence the reader will consume.
    delivered: u32,
    /// Next sequence not yet contiguously received. This is the acknowledgement.
    cumulative: u32,
    /// One past the highest sequence seen, for gap detection.
    highest: u32,
}

impl<'a> RecvRing<'a> {
    /// Build a ring over caller-owned storage.
    ///
    /// `bodies` must be exactly `meta.len() * slot_len` bytes.
    pub fn new(bodies: &'a mut [u8], meta: &'a mut [SlotMeta], slot_len: usize) -> Result<Self> {
        if slot_len == 0 || meta.is_empty() {
            return Err(Error::BadLength);
        }
        if bodies.len() != meta.len().checked_mul(slot_len).ok_or(Error::BadLength)? {
            return Err(Error::BadLength);
        }
        Ok(Self {
            bodies,
            meta,
            slot_len,
            delivered: 0,
            cumulative: 0,
            highest: 0,
        })
    }

    /// Start the channel at `seq` rather than zero.
    pub fn reset_to(&mut self, start: u32) {
        for slot in self.meta.iter_mut() {
            *slot = SlotMeta::default();
        }
        self.delivered = start;
        self.cumulative = start;
        self.highest = start;
    }

    /// Next sequence expected contiguously. This is what we acknowledge.
    pub fn cumulative_ack(&self) -> u32 {
        self.cumulative
    }

    /// Where the reader has consumed to.
    pub fn delivered(&self) -> u32 {
        self.delivered
    }

    /// True if something arrived past the contiguous frontier, so a fragment is
    /// missing. This is what justifies setting the negative acknowledgement bit.
    pub fn has_gap(&self) -> bool {
        self.cumulative != self.highest
    }

    /// Slots the ring holds.
    pub fn slots(&self) -> usize {
        self.meta.len()
    }

    fn index(&self, sequence: u32) -> usize {
        (sequence as usize) % self.meta.len()
    }

    fn slot_body(&self, sequence: u32) -> Option<&[u8]> {
        let index = self.index(sequence);
        let meta = self.meta.get(index)?;
        if !meta.ready {
            return None;
        }
        let start = index.checked_mul(self.slot_len)?;
        self.bodies
            .get(start..start.checked_add(meta.len as usize)?)
    }

    /// Offer a fragment to the ring.
    pub fn store(&mut self, sequence: u32, body: &[u8]) -> Stored {
        if seq::lt(sequence, self.delivered) {
            return Stored::Duplicate;
        }
        if seq::distance(self.delivered, sequence) as usize >= self.meta.len() {
            return Stored::OutOfWindow;
        }
        if body.len() > self.slot_len {
            return Stored::TooLarge;
        }
        let Ok(len) = u16::try_from(body.len()) else {
            return Stored::TooLarge;
        };

        let index = self.index(sequence);
        let Some(meta) = self.meta.get_mut(index) else {
            return Stored::OutOfWindow;
        };
        if meta.ready {
            return Stored::Duplicate;
        }

        let Some(start) = index.checked_mul(self.slot_len) else {
            return Stored::OutOfWindow;
        };
        let Some(slot) = self.bodies.get_mut(start..start + body.len()) else {
            return Stored::OutOfWindow;
        };
        slot.copy_from_slice(body);

        let Some(meta) = self.meta.get_mut(index) else {
            return Stored::OutOfWindow;
        };
        meta.len = len;
        meta.ready = true;

        let next = sequence.wrapping_add(1);
        if seq::gt(next, self.highest) {
            self.highest = next;
        }
        self.advance();
        Stored::Accepted
    }

    /// Walk the contiguous frontier forward over ready slots.
    fn advance(&mut self) {
        while self.cumulative != self.highest {
            let index = self.index(self.cumulative);
            match self.meta.get(index) {
                Some(meta) if meta.ready => {}
                _ => break,
            }
            self.cumulative = self.cumulative.wrapping_add(1);
        }
    }

    /// Take the next complete message, writing its **content** into `out`.
    ///
    /// The four-byte length prefix is consumed here and does not appear in the
    /// output. Returns `None` when no complete message is available, which
    /// covers both an empty channel and a message whose tail has not arrived.
    ///
    /// Completeness is decided by accumulating fragment lengths until they
    /// reach the declared total. That needs no knowledge of the sender's
    /// fragment size, and it is why the last-fragment flag plays no part.
    pub fn take_message(&mut self, out: &mut [u8]) -> Option<Result<usize>> {
        if self.delivered == self.cumulative {
            return None;
        }
        let first = self.slot_body(self.delivered)?;
        let total = match message::parse_length_prefix(first) {
            Ok(total) => total as usize,
            Err(error) => return Some(Err(error)),
        };
        let stream = total.checked_add(LENGTH_PREFIX_LEN)?;

        let mut collected = 0usize;
        let mut fragments = 0u32;
        let mut cursor = self.delivered;
        while collected < stream {
            if cursor == self.cumulative {
                // The tail has not arrived yet. Not an error.
                return None;
            }
            let body = self.slot_body(cursor)?;
            collected = collected.checked_add(body.len())?;
            fragments = fragments.checked_add(1)?;
            cursor = cursor.wrapping_add(1);
        }
        if collected != stream {
            return Some(Err(Error::BadLength));
        }
        if out.len() < total {
            return Some(Err(Error::BufferTooSmall));
        }

        let mut written = 0usize;
        let mut skip = LENGTH_PREFIX_LEN;
        let mut cursor = self.delivered;
        while written < total {
            let Some(body) = self.slot_body(cursor) else {
                return Some(Err(Error::BadLength));
            };
            let src = body.get(skip..).unwrap_or(&[]);
            let take = src.len().min(total - written);
            let (Some(dst), Some(src)) = (out.get_mut(written..written + take), src.get(..take))
            else {
                return Some(Err(Error::BufferTooSmall));
            };
            dst.copy_from_slice(src);
            written += take;
            skip = 0;
            cursor = cursor.wrapping_add(1);
        }

        for step in 0..fragments {
            let index = self.index(self.delivered.wrapping_add(step));
            if let Some(meta) = self.meta.get_mut(index) {
                *meta = SlotMeta::default();
            }
        }
        self.delivered = self.delivered.wrapping_add(fragments);

        Some(Ok(total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const SLOT: usize = 64;
    const SLOTS: usize = 8;

    struct Storage {
        bodies: [u8; SLOT * SLOTS],
        meta: [SlotMeta; SLOTS],
    }

    impl Storage {
        fn new() -> Self {
            Self {
                bodies: [0u8; SLOT * SLOTS],
                meta: [SlotMeta::default(); SLOTS],
            }
        }
        fn ring(&mut self) -> RecvRing<'_> {
            RecvRing::new(&mut self.bodies, &mut self.meta, SLOT).unwrap()
        }
    }

    /// Build a message's fragments: prefix then content, split at `capacity`.
    fn fragments(content: &[u8], capacity: usize) -> Vec<Vec<u8>> {
        let mut stream = (content.len() as u32).to_be_bytes().to_vec();
        stream.extend_from_slice(content);
        stream.chunks(capacity).map(<[u8]>::to_vec).collect()
    }

    #[test]
    fn a_single_fragment_message_round_trips() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let parts = fragments(b"hello", SLOT);
        assert_eq!(ring.store(0, &parts[0]), Stored::Accepted);
        assert_eq!(ring.cumulative_ack(), 1);

        let mut out = [0u8; 64];
        assert_eq!(ring.take_message(&mut out).unwrap().unwrap(), 5);
        assert_eq!(&out[..5], b"hello");
        assert_eq!(ring.delivered(), 1);
        assert!(ring.take_message(&mut out).is_none());
    }

    #[test]
    fn a_multi_fragment_message_reassembles() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let content: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let parts = fragments(&content, SLOT);
        assert_eq!(parts.len(), 4);
        for (index, part) in parts.iter().enumerate() {
            assert_eq!(ring.store(index as u32, part), Stored::Accepted);
        }
        let mut out = [0u8; 256];
        assert_eq!(ring.take_message(&mut out).unwrap().unwrap(), 200);
        assert_eq!(&out[..200], &content[..]);
        assert_eq!(ring.delivered(), 4);
    }

    /// The tail has not arrived: not an error, just nothing to deliver yet.
    #[test]
    fn an_incomplete_message_yields_nothing() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let content: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let parts = fragments(&content, SLOT);
        for part in parts.iter().take(3) {
            ring.store_next(part);
        }
        let mut out = [0u8; 256];
        assert!(ring.take_message(&mut out).is_none());
        ring.store(3, &parts[3]);
        assert_eq!(ring.take_message(&mut out).unwrap().unwrap(), 200);
    }

    #[test]
    fn out_of_order_arrival_still_reassembles() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let content: Vec<u8> = (0..150u32).map(|i| i as u8).collect();
        let parts = fragments(&content, SLOT);
        // Deliberately reversed.
        for index in (0..parts.len()).rev() {
            ring.store(index as u32, &parts[index]);
        }
        assert!(!ring.has_gap());
        let mut out = [0u8; 256];
        assert_eq!(ring.take_message(&mut out).unwrap().unwrap(), 150);
        assert_eq!(&out[..150], &content[..]);
    }

    #[test]
    fn a_gap_is_visible_and_blocks_the_acknowledgement() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let parts = fragments(b"x", SLOT);
        ring.store(1, &parts[0]);
        assert!(ring.has_gap());
        assert_eq!(ring.cumulative_ack(), 0, "ack must not pass the gap");
        ring.store(0, &parts[0]);
        assert!(!ring.has_gap());
        assert_eq!(ring.cumulative_ack(), 2);
    }

    #[test]
    fn duplicates_are_recognised_rather_than_stored_twice() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let parts = fragments(b"abc", SLOT);
        assert_eq!(ring.store(0, &parts[0]), Stored::Accepted);
        assert_eq!(ring.store(0, &parts[0]), Stored::Duplicate);
        let mut out = [0u8; 32];
        ring.take_message(&mut out).unwrap().unwrap();
        // Delivered, so a retransmission is still a duplicate and never
        // overwrites a slot the reader has moved past.
        assert_eq!(ring.store(0, &parts[0]), Stored::Duplicate);
    }

    #[test]
    fn the_window_is_bounded_by_the_reader_not_the_acknowledgement() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let parts = fragments(b"y", SLOT);
        // Fill the whole window without reading.
        for index in 0..SLOTS as u32 {
            assert_eq!(ring.store(index, &parts[0]), Stored::Accepted);
        }
        assert_eq!(ring.cumulative_ack(), SLOTS as u32);
        // One past the window must be refused, even though the acknowledgement
        // has advanced: those slots hold undelivered data.
        assert_eq!(ring.store(SLOTS as u32, &parts[0]), Stored::OutOfWindow);
        // Reading one frees exactly one slot.
        let mut out = [0u8; 32];
        ring.take_message(&mut out).unwrap().unwrap();
        assert_eq!(ring.store(SLOTS as u32, &parts[0]), Stored::Accepted);
    }

    #[test]
    fn a_body_larger_than_a_slot_is_refused() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let oversized = [0u8; SLOT + 1];
        assert_eq!(ring.store(0, &oversized), Stored::TooLarge);
    }

    #[test]
    fn indices_wrap_without_losing_messages() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let parts = fragments(b"wrap", SLOT);
        let mut out = [0u8; 32];
        for round in 0..(SLOTS * 5) as u32 {
            assert_eq!(ring.store(round, &parts[0]), Stored::Accepted);
            assert_eq!(ring.take_message(&mut out).unwrap().unwrap(), 4);
            assert_eq!(&out[..4], b"wrap");
        }
    }

    #[test]
    fn reset_moves_both_cursors_and_clears_slots() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        let parts = fragments(b"z", SLOT);
        ring.store(0, &parts[0]);
        ring.reset_to(100);
        assert_eq!(ring.cumulative_ack(), 100);
        assert_eq!(ring.delivered(), 100);
        assert!(!ring.has_gap());
        let mut out = [0u8; 32];
        assert!(ring.take_message(&mut out).is_none());
        assert_eq!(ring.store(100, &parts[0]), Stored::Accepted);
    }

    #[test]
    fn a_truncated_first_fragment_reports_bad_length() {
        let mut storage = Storage::new();
        let mut ring = storage.ring();
        ring.store(0, &[1, 2, 3]);
        let mut out = [0u8; 32];
        assert_eq!(ring.take_message(&mut out), Some(Err(Error::BadLength)));
    }

    #[test]
    fn rejects_mismatched_storage() {
        let mut bodies = [0u8; 10];
        let mut meta = [SlotMeta::default(); 3];
        assert!(RecvRing::new(&mut bodies, &mut meta, 4).is_err());
        let mut empty: [SlotMeta; 0] = [];
        assert!(RecvRing::new(&mut bodies, &mut empty, 4).is_err());
    }

    impl RecvRing<'_> {
        /// Test helper: store at the current contiguous frontier.
        fn store_next(&mut self, body: &[u8]) -> Stored {
            let at = self.cumulative;
            self.store(at, body)
        }
    }
}
