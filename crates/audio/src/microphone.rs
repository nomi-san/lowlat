//! Decoding a guest's microphone.
//!
//! **The first thing in this system to parse bytes a peer chose.** Everything
//! else a guest sends is a fixed-shape message a few bytes long; this is a
//! codec, and a codec is a parser with a large surface. So the decoder is held
//! at arm's length: what it is asked to produce is bounded before it runs, and
//! what it may do on the way out is contained.
//!
//! The output is always samples. A guest picks the encoding and a host does
//! not have to care which it picked ([06 §13](../../../docs/06-api.md)).

use lowlat_core::microphone::{Encoding, Packet, SAMPLES_MAX};
use opus_rs::OpusDecoder;

use crate::Error;

/// One guest's microphone, decoded.
///
/// **One per guest, because a codec carries state between packets.** Feeding
/// two guests' packets to one decoder produces sound that is neither guest's.
pub struct Decoder {
    /// **Absent after a decode that ended badly**, and rebuilt on the next
    /// packet: state that a panic unwound through is state nothing should read
    /// again.
    inner: Option<OpusDecoder>,
    /// Where the codec writes, allocated once. It produces floats and the
    /// boundary hands over samples.
    scratch: Vec<f32>,
    /// Packets this guest sent that could not be decoded.
    refused: u64,
    /// How many of those ended in a panic rather than an error.
    ///
    /// **Counted apart from the rest, because the two mean different things.**
    /// A refusal is a codec reading a packet and saying no; this is a codec
    /// that did not return, and it is the number that says whether the
    /// containment is load bearing on real traffic.
    panicked: u64,
}

impl core::fmt::Debug for Decoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Decoder")
            .field("built", &self.inner.is_some())
            .field("refused", &self.refused)
            .field("panicked", &self.panicked)
            .finish()
    }
}

impl Decoder {
    /// Build one for a guest.
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            inner: Some(build()?),
            scratch: vec![0.0; SAMPLES_MAX],
            refused: 0,
            panicked: 0,
        })
    }

    /// How many of this guest's packets were refused.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// How many of them ended in a panic the containment caught.
    pub fn panicked(&self) -> u64 {
        self.panicked
    }

    /// Decode one packet into `out`, returning how many samples it produced.
    ///
    /// `out` must hold [`SAMPLES_MAX`]; nothing is asked for beyond that, so a
    /// packet claiming to carry more is refused rather than served.
    pub fn decode(&mut self, packet: &Packet<'_>, out: &mut [i16]) -> Result<usize, Error> {
        let room = out.len().min(SAMPLES_MAX);
        match packet.encoding {
            Encoding::Raw => take_samples(packet.payload, out, room),
            Encoding::Compressed => self.decompress(packet.payload, out, room),
        }
        .inspect_err(|_| self.refused = self.refused.saturating_add(1))
    }

    fn decompress(&mut self, payload: &[u8], out: &mut [i16], room: usize) -> Result<usize, Error> {
        if payload.is_empty() {
            return Err(Error::Encode);
        }
        let mut decoder = match self.inner.take() {
            Some(decoder) => decoder,
            // Rebuilt here rather than at the failure, so a guest that sends
            // nothing more costs nothing more.
            None => build()?,
        };
        let scratch = &mut self.scratch;
        // **Contained, and the state is thrown away rather than reused.** This
        // decoder is a port that has been measured to panic on a malformed
        // packet, and these packets are a guest's to malform; a panic crossing
        // into the loop that called this would end the session, and one caught
        // and then decoded against again would be reading whatever the unwind
        // left behind.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decoder.decode(payload, room, scratch.as_mut_slice())
        }));
        match outcome {
            Ok(Ok(samples)) => {
                self.inner = Some(decoder);
                let samples = samples.min(room);
                let source = self.scratch.get(..samples).ok_or(Error::Encode)?;
                let target = out.get_mut(..samples).ok_or(Error::Encode)?;
                for (slot, sample) in target.iter_mut().zip(source) {
                    *slot = to_sample(*sample);
                }
                Ok(samples)
            }
            Ok(Err(_)) => {
                // Refused cleanly: the state is still its own, so it is kept.
                self.inner = Some(decoder);
                Err(Error::Encode)
            }
            Err(_) => {
                self.panicked = self.panicked.saturating_add(1);
                lowlat_common::log_warn!(
                    "audio: a microphone packet was refused by the decoder, panicked={}",
                    self.panicked
                );
                Err(Error::Encode)
            }
        }
    }
}

fn build() -> Result<OpusDecoder, Error> {
    OpusDecoder::new(
        i32::try_from(lowlat_core::microphone::SAMPLE_RATE).map_err(|_| Error::Encode)?,
        lowlat_core::microphone::CHANNELS,
    )
    .map_err(|_| Error::Encode)
}

/// Uncompressed is already samples, and the only work is the byte order.
fn take_samples(payload: &[u8], out: &mut [i16], room: usize) -> Result<usize, Error> {
    let samples = payload.len() / 2;
    if samples > room {
        return Err(Error::Encode);
    }
    let target = out.get_mut(..samples).ok_or(Error::Encode)?;
    for (slot, pair) in target.iter_mut().zip(payload.chunks_exact(2)) {
        let [low, high] = <[u8; 2]>::try_from(pair).map_err(|_| Error::Encode)?;
        *slot = i16::from_le_bytes([low, high]);
    }
    Ok(samples)
}

/// One float to one sample, clamped rather than wrapped.
///
/// **A codec may hand back more than full scale**, and a cast that wrapped
/// would turn the loudest moment of a word into the quietest.
#[allow(
    clippy::cast_possible_truncation,
    reason = "clamped to the sample range on the line above the cast"
)]
fn to_sample(value: f32) -> i16 {
    let scaled = (value * 32767.0).clamp(-32768.0, 32767.0);
    scaled as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use lowlat_core::microphone::Packet;

    fn decoder() -> Decoder {
        Decoder::new().expect("a decoder")
    }

    /// Uncompressed samples arrive as they were sent.
    #[test]
    fn uncompressed_samples_pass_through() {
        let sent: [i16; 4] = [0, 1000, -1000, i16::MAX];
        let mut payload = Vec::new();
        for sample in sent {
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        let mut out = [0i16; SAMPLES_MAX];
        let taken = decoder()
            .decode(
                &Packet {
                    payload: &payload,
                    encoding: Encoding::Raw,
                },
                &mut out,
            )
            .expect("samples");
        assert_eq!(taken, sent.len());
        assert_eq!(&out[..taken], &sent);
    }

    /// **What a guest sends cannot ask for more than the bound.** The length
    /// is a peer's to write, and a receiver that sized its work from it would
    /// be taking instructions from the far side.
    #[test]
    fn uncompressed_past_the_bound_is_refused() {
        let payload = vec![0u8; (SAMPLES_MAX + 1) * 2];
        let mut out = [0i16; SAMPLES_MAX];
        assert!(
            decoder()
                .decode(
                    &Packet {
                        payload: &payload,
                        encoding: Encoding::Raw,
                    },
                    &mut out,
                )
                .is_err()
        );
    }

    /// **Nothing a guest can send may end the session**, whatever it does to
    /// the codec -- and this proves the containment runs rather than assuming
    /// it. Sweeping random payloads with a fixed seed, seventeen of forty
    /// thousand ended in a panic rather than an error; each one of those would
    /// have taken a guest's session with it.
    ///
    /// **The panic is a property of the decoder's state, not of one packet.**
    /// The same bytes handed to a fresh decoder decode without complaint,
    /// which is why the reproducer here is a sequence rather than a packet --
    /// and why a fuzz target for this has to feed sequences too.
    #[test]
    fn a_packet_that_panics_the_codec_is_contained_and_the_decoder_survives() {
        let mut decoder = decoder();
        let mut out = [0i16; SAMPLES_MAX];
        let mut seed = 0x00C0_FFEEu32;
        'sweep: for length in 1..=200usize {
            for _ in 0..200 {
                let payload: Vec<u8> = (0..length)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 24) as u8
                    })
                    .collect();
                let _ = decoder.decode(
                    &Packet {
                        payload: &payload,
                        encoding: Encoding::Compressed,
                    },
                    &mut out,
                );
                if decoder.panicked() > 0 {
                    break 'sweep;
                }
            }
        }
        assert!(
            decoder.panicked() > 0,
            "the containment never ran, so this test proves nothing"
        );

        // **And the guest is not finished.** One packet that ended badly must
        // not cost the rest of the conversation, which is the half a bare
        // catch gets wrong: the state it unwound through is thrown away and
        // the next packet builds a decoder that never saw it.
        let samples: [i16; 3] = [4, 5, 6];
        let mut good = Vec::new();
        for sample in samples {
            good.extend_from_slice(&sample.to_le_bytes());
        }
        let taken = decoder
            .decode(
                &Packet {
                    payload: &good,
                    encoding: Encoding::Raw,
                },
                &mut out,
            )
            .expect("the decoder did not recover");
        assert_eq!(&out[..taken], &samples);
    }

    /// An empty payload is refused rather than handed to the codec, which
    /// treats it as a lost frame and invents sound for it.
    #[test]
    fn an_empty_compressed_payload_is_refused() {
        let mut out = [0i16; SAMPLES_MAX];
        assert!(
            decoder()
                .decode(
                    &Packet {
                        payload: &[],
                        encoding: Encoding::Compressed,
                    },
                    &mut out,
                )
                .is_err()
        );
    }

    /// Full scale stays full scale rather than wrapping to the other end of
    /// the range.
    #[test]
    fn a_loud_sample_clamps_rather_than_wraps() {
        assert_eq!(to_sample(1.5), i16::MAX);
        assert_eq!(to_sample(-1.5), i16::MIN);
        // Full scale in, full scale out, and the negative end reaches one
        // further than the positive one because the range is not symmetric.
        assert_eq!(to_sample(1.0), i16::MAX);
        assert_eq!(to_sample(-1.0), i16::MIN + 1);
        assert_eq!(to_sample(0.0), 0);
    }
}
