//! Message framing and fragmentation (docs/01-protocol.md 5.3).
//!
//! A channel carries messages, not packets. A message is a virtual byte stream
//!
//! ```text
//! [u32 be total_length][caller header][payload]
//! ```
//!
//! sliced into fragments of `body_capacity` bytes and placed on consecutive
//! sequence numbers. `total_length` counts the header and payload and excludes
//! its own four bytes.
//!
//! Reassembly is driven by that length, never by the last-fragment flag. A
//! reassembler that stopped at the flag would work against a well-behaved
//! sender and fail exactly when a tail is truncated or reordered.

use crate::error::{Error, Result};

/// Width of the length prefix on the first fragment.
pub const LENGTH_PREFIX_LEN: usize = 4;

/// How many fragments a message of `total_len` occupies.
///
/// The prefix counts toward the first fragment's body, which is why the `+ 4`
/// is inside the division rather than outside it.
pub const fn fragment_count(total_len: u32, body_capacity: usize) -> usize {
    if body_capacity == 0 {
        return 0;
    }
    let stream = total_len as usize + LENGTH_PREFIX_LEN;
    let whole = stream / body_capacity;
    if stream % body_capacity == 0 {
        whole
    } else {
        whole + 1
    }
}

/// Read the length prefix from the first fragment's body.
pub fn parse_length_prefix(first_body: &[u8]) -> Result<u32> {
    first_body
        .get(..LENGTH_PREFIX_LEN)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_be_bytes)
        .ok_or(Error::BadLength)
}

/// An outbound message, viewed as the byte stream it will be sliced into.
///
/// Holds borrows rather than copying, so building one allocates nothing and
/// costs nothing until fragments are actually written.
#[derive(Debug, Clone, Copy)]
pub struct Message<'a> {
    prefix: [u8; LENGTH_PREFIX_LEN],
    header: &'a [u8],
    payload: &'a [u8],
}

/// What [`Message::fragment`] produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fragment {
    /// Bytes written into the caller's buffer.
    pub len: usize,
    /// Whether this fragment carries the last-fragment flag.
    pub last: bool,
}

impl<'a> Message<'a> {
    /// Build a message from its header and payload.
    pub fn new(header: &'a [u8], payload: &'a [u8]) -> Result<Self> {
        let total = header
            .len()
            .checked_add(payload.len())
            .ok_or(Error::BadLength)?;
        let total = u32::try_from(total).map_err(|_| Error::BadLength)?;
        Ok(Self {
            prefix: total.to_be_bytes(),
            header,
            payload,
        })
    }

    /// Declared length: header plus payload, excluding the prefix itself.
    pub fn total_len(&self) -> u32 {
        u32::from_be_bytes(self.prefix)
    }

    /// Length of the whole virtual stream, prefix included.
    pub fn stream_len(&self) -> usize {
        LENGTH_PREFIX_LEN + self.header.len() + self.payload.len()
    }

    /// Fragments this message needs at `body_capacity`.
    pub fn fragment_count(&self, body_capacity: usize) -> usize {
        fragment_count(self.total_len(), body_capacity)
    }

    /// Write fragment `index` into `out`.
    ///
    /// Returns `None` when `index` is past the end, so a caller can drive this
    /// with a plain loop and stop naturally.
    pub fn fragment(
        &self,
        index: usize,
        body_capacity: usize,
        out: &mut [u8],
    ) -> Option<Result<Fragment>> {
        if body_capacity == 0 {
            return Some(Err(Error::BadLength));
        }
        let stream_len = self.stream_len();
        let start = index.checked_mul(body_capacity)?;
        if start >= stream_len {
            // An empty message still has exactly one fragment: the prefix.
            return None;
        }
        let end = (start + body_capacity).min(stream_len);
        let len = end - start;
        let Some(dst) = out.get_mut(..len) else {
            return Some(Err(Error::BufferTooSmall));
        };

        for (offset, byte) in dst.iter_mut().enumerate() {
            *byte = match self.byte_at(start + offset) {
                Some(value) => value,
                None => return Some(Err(Error::BadLength)),
            };
        }

        Some(Ok(Fragment {
            len,
            last: end == stream_len,
        }))
    }

    /// The virtual stream's byte at `position`.
    fn byte_at(&self, position: usize) -> Option<u8> {
        if position < LENGTH_PREFIX_LEN {
            return self.prefix.get(position).copied();
        }
        let position = position - LENGTH_PREFIX_LEN;
        if position < self.header.len() {
            return self.header.get(position).copied();
        }
        self.payload.get(position - self.header.len()).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default wire size gives 1193 bytes of body per fragment.
    const CAPACITY: usize = 1193;
    const CAPACITY_U32: u32 = 1193;

    fn collect(message: &Message<'_>, capacity: usize, out: &mut [u8]) -> (usize, usize) {
        let mut written = 0;
        let mut fragments = 0;
        let mut scratch = [0u8; 4096];
        while let Some(result) = message.fragment(fragments, capacity, &mut scratch) {
            let fragment = result.unwrap();
            let expected_last = fragments + 1 == message.fragment_count(capacity);
            assert_eq!(fragment.last, expected_last, "fragment {fragments}");
            out[written..written + fragment.len].copy_from_slice(&scratch[..fragment.len]);
            written += fragment.len;
            fragments += 1;
        }
        (fragments, written)
    }

    #[test]
    fn a_short_message_is_one_fragment() {
        let message = Message::new(b"header", b"payload").unwrap();
        assert_eq!(message.total_len(), 13);
        assert_eq!(message.fragment_count(CAPACITY), 1);

        let mut out = [0u8; 64];
        let (fragments, written) = collect(&message, CAPACITY, &mut out);
        assert_eq!(fragments, 1);
        assert_eq!(written, 4 + 13);
        assert_eq!(parse_length_prefix(&out).unwrap(), 13);
        assert_eq!(&out[4..17], b"headerpayload");
    }

    #[test]
    fn an_empty_message_still_carries_its_prefix() {
        let message = Message::new(&[], &[]).unwrap();
        assert_eq!(message.total_len(), 0);
        assert_eq!(message.fragment_count(CAPACITY), 1);
        let mut out = [0u8; 16];
        let (fragments, written) = collect(&message, CAPACITY, &mut out);
        assert_eq!((fragments, written), (1, 4));
        assert_eq!(parse_length_prefix(&out).unwrap(), 0);
    }

    #[test]
    fn fragments_reassemble_to_the_original_stream() {
        let header = [0xABu8; 10];
        let payload = [0x5Au8; 5000];
        let message = Message::new(&header, &payload).unwrap();
        assert_eq!(message.total_len(), 5010);
        assert_eq!(message.fragment_count(CAPACITY), 5);

        let mut out = [0u8; 8192];
        let (fragments, written) = collect(&message, CAPACITY, &mut out);
        assert_eq!(fragments, 5);
        assert_eq!(written, 4 + 5010);
        assert_eq!(parse_length_prefix(&out).unwrap(), 5010);
        assert_eq!(&out[4..14], &header);
        assert_eq!(&out[14..written], &payload[..]);
    }

    /// The boundary the arithmetic exists for: a stream that divides evenly
    /// must not gain a trailing empty fragment.
    #[test]
    fn an_exactly_divisible_stream_gains_no_extra_fragment() {
        let payload_len = CAPACITY * 3 - LENGTH_PREFIX_LEN;
        let payload = [0u8; 1193 * 3];
        let message = Message::new(&[], &payload[..payload_len]).unwrap();
        assert_eq!(message.stream_len(), CAPACITY * 3);
        assert_eq!(message.fragment_count(CAPACITY), 3);

        let mut scratch = [0u8; 4096];
        assert!(message.fragment(2, CAPACITY, &mut scratch).is_some());
        assert!(message.fragment(3, CAPACITY, &mut scratch).is_none());
        let last = message
            .fragment(2, CAPACITY, &mut scratch)
            .unwrap()
            .unwrap();
        assert!(last.last);
        assert_eq!(last.len, CAPACITY);
    }

    #[test]
    fn one_byte_past_a_boundary_adds_a_fragment() {
        assert_eq!(fragment_count(CAPACITY_U32 - 4, CAPACITY), 1);
        assert_eq!(fragment_count(CAPACITY_U32 - 3, CAPACITY), 2);
    }

    /// The corpus contains a message spanning 529 fragments; the arithmetic
    /// must hold at that scale and not just for small cases.
    #[test]
    fn matches_the_largest_observed_message() {
        let count = fragment_count(529 * CAPACITY_U32 - 4, CAPACITY);
        assert_eq!(count, 529);
        assert_eq!(fragment_count(528 * CAPACITY_U32, CAPACITY), 529);
    }

    #[test]
    fn a_short_first_body_has_no_length_prefix() {
        assert_eq!(parse_length_prefix(&[1, 2, 3]), Err(Error::BadLength));
    }
}
