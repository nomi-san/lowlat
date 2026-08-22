//! Audio message framing (docs/01-protocol.md 11.4).
//!
//! Fifteen bytes ahead of the payload, inside the ordinary message framing, so
//! every offset here is relative to the message **content**: what follows the
//! four-byte length prefix.
//!
//! ```text
//! 0  4  channel mask, little endian
//! 4  4  samples per channel in this packet, little endian
//! 8  4  sample rate, little endian, always 48000
//! 12 1  codec
//! 13 1  written as 2 and never read
//! 14 1  channel count
//! ```
//!
//! Little endian throughout, as in the video header and unlike the sequence
//! numbers and lengths around it.
//!
//! **Three of these fields rebuild a receiver's decoder when they change**: the
//! codec, the channel count and the mask. Nothing else does, which is what
//! makes changing sound device mid-session free and changing layout expensive.

use crate::error::{Error, Result};

/// Bytes of header ahead of the payload.
pub const AUDIO_HEADER_LEN: usize = 15;

/// The only rate this protocol carries.
pub const SAMPLE_RATE: u32 = 48000;

/// Front left and front right, which is the layout this host sends.
///
/// **Only the low-frequency bit within the mask is consulted** by a receiver,
/// and this does not set it: two channels without it select one stream and one
/// coupled pair, which is what an ordinary stereo encoder produces. A mask of
/// zero would select the same thing, so emitting the true value costs nothing
/// and describes the payload rather than relying on that.
pub const STEREO_MASK: u32 = 0x3;

/// The byte at offset 13, which a receiver does not read.
///
/// Emitted as the value every recorded stream carries. It sits between the
/// codec and the channel count, and reading the pair as one 16-bit tag is the
/// mistake this constant exists to prevent: the two bytes are unrelated and
/// only look like a tag because stereo makes both of them two.
const UNREAD_BYTE: u8 = 2;

/// How the payload is encoded.
///
/// **Per packet, not per session.** A guest that asks for uncompressed and one
/// that did not can be served from the same capture in the same room, each
/// getting the packet its own declaration asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Codec {
    /// Compressed. **Any value that is not [`Codec::Pcm`] means this**, which
    /// is why parsing cannot be a table lookup.
    Opus = 1,
    /// Interleaved sixteen-bit samples, native endianness of every platform
    /// this runs on.
    Pcm = 2,
}

impl Codec {
    /// Read the byte at offset 12.
    ///
    /// **Everything that is not 2 is compressed.** A receiver tests for the
    /// uncompressed value and treats the rest as Opus, so a host reading this
    /// as an enumeration of two known values would disagree with it about any
    /// third.
    pub const fn from_bits(bits: u8) -> Self {
        match bits {
            2 => Codec::Pcm,
            _ => Codec::Opus,
        }
    }

    /// Bytes one sample of this codec occupies per channel, if it is fixed.
    const fn sample_bytes(self) -> Option<usize> {
        match self {
            Codec::Pcm => Some(2),
            Codec::Opus => None,
        }
    }
}

/// One audio packet's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioHeader {
    /// Which speakers the channels belong to. A change rebuilds a receiver's
    /// decoder.
    pub mask: u32,
    /// **Per channel, not the total.** 960 is the 20 ms packet this host
    /// sends.
    pub samples: u32,
    /// Always [`SAMPLE_RATE`]. Carried rather than assumed because a receiver
    /// reads it.
    pub rate: u32,
    pub codec: Codec,
    /// A change rebuilds a receiver's decoder, so it describes the payload and
    /// is never a constant written once.
    pub channels: u8,
}

impl AudioHeader {
    /// The header this host emits for one packet of stereo.
    pub const fn stereo(samples: u32, codec: Codec) -> Self {
        Self {
            mask: STEREO_MASK,
            samples,
            rate: SAMPLE_RATE,
            codec,
            channels: 2,
        }
    }

    /// How many bytes an uncompressed payload of this many samples occupies.
    ///
    /// `None` for a compressed one, whose length is whatever the encoder
    /// produced and is not derivable from the header.
    pub const fn payload_len(&self) -> Option<usize> {
        let Some(width) = self.codec.sample_bytes() else {
            return None;
        };
        Some(self.samples as usize * self.channels as usize * width)
    }
}

/// **A receiver refuses an uncompressed payload larger than this**, so a packet
/// longer than about 160 ms of stereo cannot be delivered uncompressed at all.
/// The 20 ms this host sends is 3840 bytes.
pub const PCM_PAYLOAD_MAX: usize = 32000;

fn le32(src: &[u8], offset: usize) -> Result<u32> {
    src.get(offset..offset + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::ShortPacket)
}

/// Parse the header from an audio message's content.
pub fn parse(content: &[u8]) -> Result<AudioHeader> {
    let mask = le32(content, 0)?;
    let samples = le32(content, 4)?;
    let rate = le32(content, 8)?;
    let &codec = content.get(12).ok_or(Error::ShortPacket)?;
    let &channels = content.get(14).ok_or(Error::ShortPacket)?;
    Ok(AudioHeader {
        mask,
        samples,
        rate,
        codec: Codec::from_bits(codec),
        channels,
    })
}

/// Write the header into the start of an audio message's content.
pub fn encode(out: &mut [u8], header: &AudioHeader) -> Result<usize> {
    let out = out
        .get_mut(..AUDIO_HEADER_LEN)
        .ok_or(Error::BufferTooSmall)?;
    let [m0, m1, m2, m3] = header.mask.to_le_bytes();
    let [s0, s1, s2, s3] = header.samples.to_le_bytes();
    let [r0, r1, r2, r3] = header.rate.to_le_bytes();
    let bytes = [
        m0,
        m1,
        m2,
        m3,
        s0,
        s1,
        s2,
        s3,
        r0,
        r1,
        r2,
        r3,
        header.codec as u8,
        UNREAD_BYTE,
        header.channels,
    ];
    out.copy_from_slice(&bytes);
    Ok(AUDIO_HEADER_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(header: &AudioHeader) -> [u8; AUDIO_HEADER_LEN] {
        let mut out = [0u8; AUDIO_HEADER_LEN];
        assert_eq!(encode(&mut out, header).expect("encodes"), AUDIO_HEADER_LEN);
        out
    }

    #[test]
    fn round_trip_is_the_identity() {
        for codec in [Codec::Opus, Codec::Pcm] {
            let header = AudioHeader::stereo(960, codec);
            assert_eq!(parse(&encoded(&header)).expect("parses"), header);
        }
    }

    /// Every field a receiver acts on, at the offset it reads.
    #[test]
    fn fields_land_where_a_receiver_reads_them() {
        let bytes = encoded(&AudioHeader::stereo(960, Codec::Opus));
        assert_eq!(&bytes[0..4], &3u32.to_le_bytes(), "mask");
        assert_eq!(&bytes[4..8], &960u32.to_le_bytes(), "samples");
        assert_eq!(&bytes[8..12], &48000u32.to_le_bytes(), "rate");
        assert_eq!(bytes[12], 1, "codec");
        assert_eq!(bytes[13], 2, "the byte nothing reads");
        assert_eq!(bytes[14], 2, "channels");
    }

    /// **Little endian, and the trap is that stereo hides it.** The mask, the
    /// channel count and the rate all survive a byte swap or read plausibly
    /// under one; the sample count does not.
    #[test]
    fn the_sample_count_is_little_endian() {
        let bytes = encoded(&AudioHeader::stereo(960, Codec::Opus));
        assert_eq!(&bytes[4..8], &[0xC0, 0x03, 0x00, 0x00]);
    }

    /// A receiver tests for the uncompressed value and treats everything else
    /// as compressed, so this must too.
    #[test]
    fn any_codec_that_is_not_two_is_compressed() {
        assert_eq!(Codec::from_bits(2), Codec::Pcm);
        for bits in [0u8, 1, 3, 255] {
            assert_eq!(Codec::from_bits(bits), Codec::Opus, "codec byte {bits}");
        }
    }

    /// The channel count comes from the header rather than from the constant
    /// beside it, which is what a reader of the pair as one tag would get
    /// wrong.
    /// **Stereo cannot show this and a test using it proves nothing**: bytes 13
    /// and 14 are both 2, so an encoder writing them in the wrong order passes
    /// every check made with two channels. Swapping them was tried and passed
    /// the whole suite until this test carried a count that is not 2.
    #[test]
    fn channels_are_written_and_read_at_their_own_byte() {
        let header = AudioHeader {
            channels: 6,
            ..AudioHeader::stereo(960, Codec::Opus)
        };
        let mut bytes = encoded(&header);
        assert_eq!(bytes[14], 6, "channels");
        assert_eq!(bytes[13], 2, "the byte nothing reads");
        assert_eq!(parse(&bytes).expect("parses").channels, 6);
        // The unread byte moving changes nothing.
        bytes[13] = 0xFF;
        assert_eq!(parse(&bytes).expect("parses").channels, 6);
    }

    #[test]
    fn an_uncompressed_payload_length_follows_from_the_header() {
        let header = AudioHeader::stereo(960, Codec::Pcm);
        assert_eq!(header.payload_len(), Some(3840));
        assert!(
            AudioHeader::stereo(960, Codec::Opus)
                .payload_len()
                .is_none()
        );
        // The whole 20 ms packet is far inside what a receiver accepts.
        assert!(header.payload_len().expect("a length") < PCM_PAYLOAD_MAX);
    }

    #[test]
    fn a_short_content_is_refused_rather_than_read_past() {
        let bytes = encoded(&AudioHeader::stereo(960, Codec::Opus));
        for len in 0..AUDIO_HEADER_LEN {
            assert!(parse(&bytes[..len]).is_err(), "at {len} bytes");
        }
        assert!(
            encode(
                &mut [0u8; AUDIO_HEADER_LEN - 1],
                &AudioHeader::stereo(960, Codec::Opus)
            )
            .is_err()
        );
    }
}
