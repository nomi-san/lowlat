//! A guest's microphone, arriving on the control channel
//! (docs/01-protocol.md 11.5).
//!
//! **Not the audio channel and not its framing.** Sound to a guest is its own
//! channel with a fifteen-byte header; sound from one rides the control
//! channel as a virtual device, which is a single opcode multiplexing several
//! kinds of device by a value in the header. Only one of those kinds is a
//! microphone and the rest are not ours to answer.
//!
//! The body is a **fixed 1932 bytes whatever it carries**, with the payload's
//! real length inside it. A sender that has ten milliseconds of sound and a
//! sender that has none write the same number of bytes.

use crate::error::{Error, Result};

/// Selects the microphone among the virtual devices, in the header's second
/// argument.
pub const MICROPHONE_SELECTOR: u32 = 0xF055_F055;

/// The header's first argument for a microphone. **Another device uses zero
/// here**, so it is part of the selection rather than a constant to ignore.
pub const MICROPHONE_ARGUMENT: u32 = 1;

/// The device kind at the head of the body.
pub const MICROPHONE_KIND: u32 = 12;

/// A virtual device body, whatever it carries.
pub const BODY_LEN: usize = 1932;

/// The most payload one can hold.
pub const PAYLOAD_MAX: usize = 1920;

const KIND_AT: usize = 0;
const PAYLOAD_AT: usize = 4;
const LENGTH_AT: usize = 1924;
const ENCODING_AT: usize = 1928;

/// Samples a second, per channel.
pub const SAMPLE_RATE: u32 = 48_000;

/// Channels. **Mono, decided by the sender**, which folds whatever its device
/// captured down before encoding.
pub const CHANNELS: usize = 1;

/// The most samples one packet may decode to.
///
/// **Ten milliseconds is what a sender produces and twenty is what a receiver
/// allows**, which is the bound rather than the frame: the codec can be asked
/// for far longer frames than that, and a length is a guest's to write.
pub const SAMPLES_MAX: usize = 960;

/// How the payload is encoded.
///
/// **The values are not the audio channel's.** That one tags compressed as 1
/// and uncompressed as 2; this one tags compressed as 1 and uncompressed as
/// **0**, and the two are close enough to swap without noticing until a
/// listener hears noise.
///
/// **Closed, unlike an opcode.** The wire defines two encodings and a third is
/// refused where the byte is read, so a reader here can be exhaustive; leaving
/// it open would put a dead arm in everything that matches on it and hide the
/// day a third appears rather than breaking the build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Sixteen-bit samples, little endian.
    Raw,
    /// Compressed, in the codec's voice mode.
    Compressed,
}

impl Encoding {
    /// Read the byte, refusing a value nobody defined.
    ///
    /// **Refused rather than defaulted**, because the two meanings differ by
    /// an order of magnitude in length: guessing wrong reads a compressed
    /// packet as samples and plays static.
    pub const fn from_bits(bits: u8) -> Result<Self> {
        match bits {
            0 => Ok(Self::Raw),
            1 => Ok(Self::Compressed),
            _ => Err(Error::Malformed),
        }
    }
}

/// One packet of a guest's microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packet<'a> {
    pub payload: &'a [u8],
    pub encoding: Encoding,
}

/// Read a virtual-device message, or `None` when it is a device this is not.
///
/// **The two header arguments select together.** The kind inside the body has
/// to agree with them, and a body that says one thing while the header says
/// another is refused rather than believed: they are written by one sender in
/// one call, so disagreement means the message is not what it claims.
pub fn parse<'a>(a0: u32, a1: u32, body: &'a [u8]) -> Result<Option<Packet<'a>>> {
    if a1 != MICROPHONE_SELECTOR {
        // Another virtual device. Not an error: this multiplexes several and
        // a host answers the ones it implements.
        return Ok(None);
    }
    if a0 != MICROPHONE_ARGUMENT {
        return Err(Error::Malformed);
    }
    let body = body.get(..BODY_LEN).ok_or(Error::ShortPacket)?;
    if le32(body, KIND_AT)? != MICROPHONE_KIND {
        return Err(Error::Malformed);
    }
    let length = le32(body, LENGTH_AT)? as usize;
    if length > PAYLOAD_MAX {
        return Err(Error::Malformed);
    }
    let encoding = Encoding::from_bits(*body.get(ENCODING_AT).ok_or(Error::ShortPacket)?)?;
    // **An odd length cannot be whole samples.** Uncompressed carries pairs of
    // bytes, and half a sample at the end is a packet that was not written by
    // anything that meant it.
    if encoding == Encoding::Raw && length % 2 != 0 {
        return Err(Error::Malformed);
    }
    let payload = body
        .get(PAYLOAD_AT..PAYLOAD_AT + length)
        .ok_or(Error::ShortPacket)?;
    Ok(Some(Packet { payload, encoding }))
}

/// Write one, which is what the tests and the simulator need.
pub fn encode(out: &mut [u8], packet: &Packet<'_>) -> Result<usize> {
    let out = out.get_mut(..BODY_LEN).ok_or(Error::BufferTooSmall)?;
    if packet.payload.len() > PAYLOAD_MAX {
        return Err(Error::BufferTooSmall);
    }
    out.fill(0);
    write32(out, KIND_AT, MICROPHONE_KIND)?;
    write32(
        out,
        LENGTH_AT,
        u32::try_from(packet.payload.len()).map_err(|_| Error::Malformed)?,
    )?;
    let encoding = match packet.encoding {
        Encoding::Raw => 0,
        Encoding::Compressed => 1,
    };
    *out.get_mut(ENCODING_AT).ok_or(Error::BufferTooSmall)? = encoding;
    out.get_mut(PAYLOAD_AT..PAYLOAD_AT + packet.payload.len())
        .ok_or(Error::BufferTooSmall)?
        .copy_from_slice(packet.payload);
    Ok(BODY_LEN)
}

/// The body's own fields are little endian, unlike the header's arguments.
fn le32(src: &[u8], at: usize) -> Result<u32> {
    src.get(at..at + 4)
        .and_then(|s| <[u8; 4]>::try_from(s).ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::ShortPacket)
}

fn write32(out: &mut [u8], at: usize, value: u32) -> Result<()> {
    out.get_mut(at..at + 4)
        .ok_or(Error::BufferTooSmall)?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_of(packet: &Packet<'_>) -> [u8; BODY_LEN] {
        let mut body = [0u8; BODY_LEN];
        encode(&mut body, packet).expect("a body");
        body
    }

    /// A packet written and read back is the one that was written.
    #[test]
    fn a_packet_survives_the_round_trip() {
        for encoding in [Encoding::Raw, Encoding::Compressed] {
            let payload = [0x11u8, 0x22, 0x33, 0x44];
            let packet = Packet {
                payload: &payload,
                encoding,
            };
            let body = body_of(&packet);
            let read = parse(MICROPHONE_ARGUMENT, MICROPHONE_SELECTOR, &body)
                .expect("a parse")
                .expect("a microphone");
            assert_eq!(read, packet);
        }
    }

    /// **The body is the same size whatever it carries**, so the length inside
    /// it is the only thing that says how much is real.
    #[test]
    fn the_body_is_a_fixed_size() {
        let quiet = body_of(&Packet {
            payload: &[],
            encoding: Encoding::Compressed,
        });
        let loud = body_of(&Packet {
            payload: &[7u8; 320],
            encoding: Encoding::Compressed,
        });
        assert_eq!(quiet.len(), loud.len());
        assert_eq!(quiet.len(), 1932);
    }

    /// **The compressed tag is 1 here and the uncompressed one is 0**, which is
    /// not what the audio channel's header uses. Reading one with the other's
    /// meaning gets uncompressed and compressed exactly backwards.
    #[test]
    fn the_encoding_tag_is_not_the_audio_channels() {
        assert_eq!(Encoding::from_bits(0), Ok(Encoding::Raw));
        assert_eq!(Encoding::from_bits(1), Ok(Encoding::Compressed));
        // The audio channel spells uncompressed 2, and that is undefined here.
        assert_eq!(Encoding::from_bits(2), Err(Error::Malformed));
        assert_eq!(crate::audio::Codec::from_bits(2), crate::audio::Codec::Pcm);
    }

    /// Another virtual device is not an error. The opcode carries several and a
    /// host answers what it implements.
    #[test]
    fn another_device_is_passed_over_rather_than_refused() {
        let body = [0u8; BODY_LEN];
        assert_eq!(parse(0, 0x056A_0357, &body), Ok(None));
    }

    /// **A length past the body is refused**, which is the field a peer would
    /// use to make a reader walk off the end.
    #[test]
    fn a_length_past_the_body_is_refused() {
        let mut body = body_of(&Packet {
            payload: &[1, 2, 3, 4],
            encoding: Encoding::Compressed,
        });
        body[LENGTH_AT..LENGTH_AT + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            parse(MICROPHONE_ARGUMENT, MICROPHONE_SELECTOR, &body),
            Err(Error::Malformed)
        );
        // And one that fits the field but not the payload.
        body[LENGTH_AT..LENGTH_AT + 4].copy_from_slice(
            &u32::try_from(PAYLOAD_MAX + 1)
                .expect("a length")
                .to_le_bytes(),
        );
        assert_eq!(
            parse(MICROPHONE_ARGUMENT, MICROPHONE_SELECTOR, &body),
            Err(Error::Malformed)
        );
    }

    /// A body shorter than the fixed size is refused rather than read at the
    /// offsets it does not have.
    #[test]
    fn a_short_body_is_refused() {
        let body = [0u8; BODY_LEN - 1];
        assert_eq!(
            parse(MICROPHONE_ARGUMENT, MICROPHONE_SELECTOR, &body),
            Err(Error::ShortPacket)
        );
    }

    /// The kind inside the body has to agree with the header that selected it.
    #[test]
    fn a_body_that_disagrees_with_its_header_is_refused() {
        let mut body = body_of(&Packet {
            payload: &[1, 2],
            encoding: Encoding::Raw,
        });
        body[KIND_AT..KIND_AT + 4].copy_from_slice(&14u32.to_le_bytes());
        assert_eq!(
            parse(MICROPHONE_ARGUMENT, MICROPHONE_SELECTOR, &body),
            Err(Error::Malformed)
        );
    }

    /// Half a sample is not a sample.
    #[test]
    fn an_odd_uncompressed_length_is_refused() {
        let mut body = body_of(&Packet {
            payload: &[1, 2, 3, 4],
            encoding: Encoding::Raw,
        });
        body[LENGTH_AT..LENGTH_AT + 4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(
            parse(MICROPHONE_ARGUMENT, MICROPHONE_SELECTOR, &body),
            Err(Error::Malformed)
        );
    }

    /// Ten milliseconds of uncompressed sound is what a sender produces, and it
    /// fits with room to spare.
    #[test]
    fn ten_milliseconds_fits_the_body() {
        let samples = SAMPLE_RATE as usize / 100 * CHANNELS;
        assert_eq!(samples, 480);
        assert!(samples * 2 < PAYLOAD_MAX);
        assert!(samples * 2 <= SAMPLES_MAX * 2);
    }
}
