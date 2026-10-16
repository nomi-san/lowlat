//! TURN framing on TCP/TLS (RFC 8656 section 12.5, RFC 8489 section 6.2.2).
//!
//! No socket, TLS engine, allocation or clock. The caller supplies bounded
//! storage and consumes each borrowed message before feeding the next bytes.
//! A read may end anywhere or contain many messages. Padding belongs to the
//! carrier, never to the peer's datagram or the ChannelData length field.

use crate::{Error, Result};

/// Largest STUN frame its aligned 16-bit length can describe.
pub const MAX_FRAME_LEN: usize = 20 + 65_532;

#[derive(Debug, Clone, Copy)]
struct Shape {
    logical: usize,
    wire: usize,
    stun: bool,
}

fn shape(header: &[u8]) -> Result<Shape> {
    let header = header.get(..4).ok_or(Error::Malformed)?;
    let high = header.first().copied().ok_or(Error::Malformed)? >> 6;
    let length = header.get(2..4).ok_or(Error::Malformed)?;
    let length = usize::from(u16::from_be_bytes(
        length.try_into().map_err(|_| Error::Malformed)?,
    ));
    match high {
        0 if length % 4 == 0 => Ok(Shape {
            logical: 20 + length,
            wire: 20 + length,
            stun: true,
        }),
        // Reserved channel numbers are framed too. The TURN receiver decides
        // whether a number is bound; dropping it must not lose stream sync.
        1 => Ok(Shape {
            logical: 4 + length,
            wire: (4 + length).next_multiple_of(4),
            stun: false,
        }),
        _ => Err(Error::Malformed),
    }
}

fn cookie(message: &[u8], frame: Shape) -> Result<()> {
    if frame.stun && message.get(4..8) != Some(&[0x21, 0x12, 0xA4, 0x42]) {
        return Err(Error::Malformed);
    }
    Ok(())
}

/// Encode one complete TURN message, adding zero ChannelData padding.
/// Accepts a logical message or an already padded ChannelData message. STUN
/// must have exactly its declared length; trailing bytes are never forwarded.
pub fn encode(message: &[u8], output: &mut [u8]) -> Result<usize> {
    let frame = shape(message)?;
    if message.len() != frame.logical && message.len() != frame.wire {
        return Err(Error::BadLength);
    }
    cookie(message, frame)?;
    let output = output.get_mut(..frame.wire).ok_or(Error::BufferTooSmall)?;
    let payload = message.get(..frame.logical).ok_or(Error::BadLength)?;
    output
        .get_mut(..frame.logical)
        .ok_or(Error::BufferTooSmall)?
        .copy_from_slice(payload);
    output
        .get_mut(frame.logical..)
        .ok_or(Error::BufferTooSmall)?
        .fill(0);
    Ok(frame.wire)
}

/// Incremental, bounded stream reader. Invalid input poisons the reader:
/// scanning for another header in an untrusted byte stream is not recovery.
#[derive(Debug)]
pub struct Decoder<B> {
    storage: B,
    used: usize,
    failed: bool,
    last: Option<usize>,
}

impl<B: AsRef<[u8]> + AsMut<[u8]>> Decoder<B> {
    /// Storage is a deliberate frame limit. At least a STUN header is needed;
    /// callers accepting every representable frame supply MAX_FRAME_LEN bytes.
    pub fn new(storage: B) -> Result<Self> {
        if storage.as_ref().len() < 20 {
            return Err(Error::BufferTooSmall);
        }
        Ok(Self {
            storage,
            used: 0,
            failed: false,
            last: None,
        })
    }

    /// Consume up to one complete frame. Returns the consumed input count and
    /// its logical message, with stream padding removed, or None if incomplete.
    /// Feed the unconsumed suffix again after handling the returned message.
    pub fn push(&mut self, input: &[u8]) -> Result<(usize, Option<&[u8]>)> {
        self.last = None;
        if self.failed {
            return Err(Error::Malformed);
        }
        let result = self.advance(input);
        let (consumed, frame) = match result {
            Ok(result) => result,
            Err(error) => {
                self.failed = true;
                return Err(error);
            }
        };
        self.last = frame;
        Ok((consumed, self.last_frame()))
    }

    /// Last complete message, until the next push. Lets an IO shell retain the
    /// owned storage and lend the message without another packet-sized copy.
    pub fn last_frame(&self) -> Option<&[u8]> {
        self.last.and_then(|len| self.storage.as_ref().get(..len))
    }

    fn advance(&mut self, input: &[u8]) -> Result<(usize, Option<usize>)> {
        let mut consumed = 0;
        if self.used < 4 {
            consumed += self.copy(input, 4)?;
            if self.used < 4 {
                return Ok((consumed, None));
            }
        }
        let frame = shape(self.storage.as_ref())?;
        if frame.wire > self.storage.as_ref().len() {
            return Err(Error::BufferTooSmall);
        }
        consumed += self.copy(input.get(consumed..).ok_or(Error::BadLength)?, frame.wire)?;
        if self.used != frame.wire {
            return Ok((consumed, None));
        }
        cookie(self.storage.as_ref(), frame)?;
        self.used = 0;
        Ok((consumed, Some(frame.logical)))
    }

    fn copy(&mut self, input: &[u8], target: usize) -> Result<usize> {
        let count = input.len().min(target.saturating_sub(self.used));
        self.storage
            .as_mut()
            .get_mut(self.used..self.used + count)
            .ok_or(Error::BufferTooSmall)?
            .copy_from_slice(input.get(..count).ok_or(Error::BadLength)?);
        self.used += count;
        Ok(count)
    }

    /// Call on EOF: a boundary is clean, a partial frame is truncated input.
    pub fn finish(&self) -> Result<()> {
        if self.failed || self.used != 0 {
            Err(Error::Malformed)
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn channel(len: u16) -> Vec<u8> {
        let mut message = vec![0x40, 0x00];
        message.extend_from_slice(&len.to_be_bytes());
        message.extend((0..len).map(|n| n as u8));
        message
    }

    fn stun() -> Vec<u8> {
        let mut message = vec![0x01, 0x03, 0, 0, 0x21, 0x12, 0xA4, 0x42];
        message.resize(20, 0);
        message
    }

    fn wire(message: &[u8]) -> Vec<u8> {
        let mut output = vec![0u8; message.len() + 3];
        let len = encode(message, &mut output).unwrap();
        output.truncate(len);
        output
    }

    #[test]
    fn every_split_point_and_padding_length_roundtrips() {
        for message in (0..8).map(channel).chain([stun()]) {
            let bytes = wire(&message);
            for split in 0..=bytes.len() {
                let mut storage = [0; 64];
                let mut decoder = Decoder::new(&mut storage).unwrap();
                let (used, frame) = decoder.push(&bytes[..split]).unwrap();
                assert_eq!(used, split);
                if split == bytes.len() {
                    assert_eq!(frame, Some(message.as_slice()));
                } else {
                    assert!(frame.is_none());
                    let (used, frame) = decoder.push(&bytes[split..]).unwrap();
                    assert_eq!(used, bytes.len() - split);
                    assert_eq!(frame, Some(message.as_slice()));
                }
                assert!(decoder.finish().is_ok());
            }
        }
    }

    #[test]
    fn coalesced_stun_and_channels_are_independent_messages() {
        let messages = [channel(1), stun(), channel(3), channel(0)];
        let bytes: Vec<_> = messages.iter().flat_map(|m| wire(m)).collect();
        let mut storage = [0; 64];
        let mut decoder = Decoder::new(&mut storage).unwrap();
        let mut offset = 0;
        for message in messages {
            let (used, frame) = decoder.push(&bytes[offset..]).unwrap();
            assert_eq!(frame, Some(message.as_slice()));
            offset += used;
        }
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn incomplete_eof_and_invalid_headers_are_not_resynchronized() {
        let bytes = wire(&channel(7));
        for end in 1..bytes.len() {
            let mut storage = [0; 64];
            let mut decoder = Decoder::new(&mut storage).unwrap();
            decoder.push(&bytes[..end]).unwrap();
            assert!(decoder.finish().is_err());
        }
        for bad in [&[0x80, 0, 0, 0][..], &[0, 0, 0, 1], &[0x40, 0, 0xFF, 0xFF]] {
            let mut storage = [0; 64];
            let mut decoder = Decoder::new(&mut storage).unwrap();
            assert!(decoder.push(bad).is_err());
            assert!(decoder.push(&wire(&stun())).is_err());
        }
    }

    #[test]
    fn cookie_lengths_and_capacity_are_checked() {
        let mut message = stun();
        message[4] = 0;
        assert!(encode(&message, &mut [0; 64]).is_err());
        let mut storage = [0; 64];
        assert!(Decoder::new(&mut storage).unwrap().push(&message).is_err());
        assert!(encode(&channel(1), &mut [0; 7]).is_err());
        let mut message = channel(1);
        message.push(0);
        assert!(encode(&message, &mut [0; 64]).is_err());
        assert!(Decoder::new(&mut [0; 19]).is_err());
    }

    #[test]
    fn maximum_representable_messages_are_bounded_and_valid() {
        let message = channel(u16::MAX);
        let bytes = wire(&message);
        let mut storage = vec![0; MAX_FRAME_LEN];
        let mut decoder = Decoder::new(&mut storage).unwrap();
        assert_eq!(decoder.push(&bytes).unwrap().1, Some(message.as_slice()));
        let mut message = stun();
        message[2..4].copy_from_slice(&65_532u16.to_be_bytes());
        message.resize(MAX_FRAME_LEN, 0);
        assert_eq!(decoder.push(&message).unwrap().1, Some(message.as_slice()));
    }
}
