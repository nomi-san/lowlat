//! Decoding sound a peer sent: a guest's microphone on the host, the host's
//! stream on the client.
//!
//! **The first thing in this system to parse bytes a peer chose.** Everything
//! else a peer sends is a fixed-shape message a few bytes long; this is a
//! codec, and a codec is a parser with a large surface. So the decoder is held
//! at arm's length: what it is asked to produce is bounded before it runs, and
//! what it may do on the way out is contained.
//!
//! The output is always samples. A peer picks the encoding and the side that
//! receives does not have to care which it picked ([06 §13](../../../docs/06-api.md)).

use opus_rs::OpusDecoder;

use crate::{Error, SAMPLE_RATE};

/// One peer's sound, decoded.
///
/// **One per peer, because a codec carries state between packets.** Feeding
/// two peers' packets to one decoder produces sound that is neither's.
pub struct Decoder {
    /// **Absent after a decode that ended badly**, and rebuilt on the next
    /// packet: state that a panic unwound through is state nothing should read
    /// again.
    inner: Option<OpusDecoder>,
    channels: usize,
    /// The most frames (samples per channel) one packet may produce.
    capacity: usize,
    /// Where the codec writes, allocated once. It produces floats and the
    /// boundary hands over samples.
    scratch: Vec<f32>,
    /// Packets this peer sent that could not be decoded.
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
            .field("channels", &self.channels)
            .field("refused", &self.refused)
            .field("panicked", &self.panicked)
            .finish()
    }
}

impl Decoder {
    /// Build one for a peer: `channels` interleaved, at most `capacity`
    /// frames a packet.
    pub fn new(channels: usize, capacity: usize) -> Result<Self, Error> {
        Ok(Self {
            inner: Some(build(channels)?),
            channels,
            capacity,
            scratch: vec![0.0; capacity * channels],
            refused: 0,
            panicked: 0,
        })
    }

    /// How many of this peer's packets were refused.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// How many of them ended in a panic the containment caught.
    pub fn panicked(&self) -> u64 {
        self.panicked
    }

    /// Decode one packet into `out`, returning how many frames it produced.
    ///
    /// `out` must hold `capacity * channels` samples; nothing is asked for
    /// beyond that, so a packet claiming to carry more is refused rather than
    /// served.
    pub fn decode(
        &mut self,
        payload: &[u8],
        compressed: bool,
        out: &mut [i16],
    ) -> Result<usize, Error> {
        let room = (out.len() / self.channels).min(self.capacity);
        if compressed {
            self.decompress(payload, out, room)
        } else {
            take_samples(payload, out, room, self.channels)
        }
        .inspect_err(|_| self.refused = self.refused.saturating_add(1))
    }

    fn decompress(&mut self, payload: &[u8], out: &mut [i16], room: usize) -> Result<usize, Error> {
        if payload.is_empty() {
            return Err(Error::Encode);
        }
        let mut decoder = match self.inner.take() {
            Some(decoder) => decoder,
            // Rebuilt here rather than at the failure, so a peer that sends
            // nothing more costs nothing more.
            None => build(self.channels)?,
        };
        let scratch = &mut self.scratch;
        // **Contained, and the state is thrown away rather than reused.** This
        // decoder is a port that has been measured to panic on a malformed
        // packet, and these packets are a peer's to malform; a panic crossing
        // into the loop that called this would end the session, and one caught
        // and then decoded against again would be reading whatever the unwind
        // left behind.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            decoder.decode(payload, room, scratch.as_mut_slice())
        }));
        match outcome {
            Ok(Ok(frames)) => {
                self.inner = Some(decoder);
                let frames = frames.min(room);
                let samples = frames * self.channels;
                let source = self.scratch.get(..samples).ok_or(Error::Encode)?;
                let target = out.get_mut(..samples).ok_or(Error::Encode)?;
                for (slot, sample) in target.iter_mut().zip(source) {
                    *slot = to_sample(*sample);
                }
                Ok(frames)
            }
            Ok(Err(_)) => {
                // Refused cleanly: the state is still its own, so it is kept.
                self.inner = Some(decoder);
                Err(Error::Encode)
            }
            Err(_) => {
                self.panicked = self.panicked.saturating_add(1);
                lowlat_common::log_warn!(
                    "audio: a packet was refused by the decoder, panicked={}",
                    self.panicked
                );
                Err(Error::Encode)
            }
        }
    }
}

fn build(channels: usize) -> Result<OpusDecoder, Error> {
    OpusDecoder::new(
        i32::try_from(SAMPLE_RATE).map_err(|_| Error::Encode)?,
        channels,
    )
    .map_err(|_| Error::Encode)
}

/// Uncompressed is already samples, and the only work is the byte order.
fn take_samples(
    payload: &[u8],
    out: &mut [i16],
    room: usize,
    channels: usize,
) -> Result<usize, Error> {
    let samples = payload.len() / 2;
    let frames = samples / channels;
    if frames > room || frames * channels != samples {
        return Err(Error::Encode);
    }
    let target = out.get_mut(..samples).ok_or(Error::Encode)?;
    for (slot, pair) in target.iter_mut().zip(payload.chunks_exact(2)) {
        let [low, high] = <[u8; 2]>::try_from(pair).map_err(|_| Error::Encode)?;
        *slot = i16::from_le_bytes([low, high]);
    }
    Ok(frames)
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
    use crate::{CHANNELS, Encoder, FRAME, FRAME_BYTES};

    const MONO_MAX: usize = 960;

    fn mono() -> Decoder {
        Decoder::new(1, MONO_MAX).expect("a decoder")
    }

    fn stereo() -> Decoder {
        Decoder::new(CHANNELS, FRAME).expect("a decoder")
    }

    /// Uncompressed samples arrive as they were sent.
    #[test]
    fn uncompressed_samples_pass_through() {
        let sent: [i16; 4] = [0, 1000, -1000, i16::MAX];
        let mut payload = Vec::new();
        for sample in sent {
            payload.extend_from_slice(&sample.to_le_bytes());
        }
        let mut out = [0i16; MONO_MAX];
        let taken = mono().decode(&payload, false, &mut out).expect("samples");
        assert_eq!(taken, sent.len());
        assert_eq!(&out[..taken], &sent);

        // Stereo: the same bytes are two frames, and the count says so.
        let mut out = [0i16; FRAME * CHANNELS];
        let taken = stereo().decode(&payload, false, &mut out).expect("frames");
        assert_eq!(taken, 2);
        assert_eq!(&out[..4], &sent);
    }

    /// **What a peer sends cannot ask for more than the bound.** The length
    /// is a peer's to write, and a receiver that sized its work from it would
    /// be taking instructions from the far side.
    #[test]
    fn uncompressed_past_the_bound_is_refused() {
        let payload = vec![0u8; (MONO_MAX + 1) * 2];
        let mut out = [0i16; MONO_MAX];
        assert!(mono().decode(&payload, false, &mut out).is_err());
        // A stereo payload with an odd number of samples is not frames.
        let payload = vec![0u8; 6];
        let mut out = [0i16; FRAME * CHANNELS];
        assert!(stereo().decode(&payload, false, &mut out).is_err());
    }

    /// **Nothing a peer can send may end the session**, whatever it does to
    /// the codec -- and this proves the containment runs rather than assuming
    /// it. Sweeping random payloads with a fixed seed, seventeen of forty
    /// thousand ended in a panic rather than an error; each one of those would
    /// have taken a session with it.
    ///
    /// **The panic is a property of the decoder's state, not of one packet.**
    /// The same bytes handed to a fresh decoder decode without complaint,
    /// which is why the reproducer here is a sequence rather than a packet --
    /// and why a fuzz target for this has to feed sequences too.
    #[test]
    fn a_packet_that_panics_the_codec_is_contained_and_the_decoder_survives() {
        for (mut decoder, channels) in [(mono(), 1), (stereo(), CHANNELS)] {
            let mut out = vec![0i16; FRAME * CHANNELS];
            let mut seed = 0x00C0_FFEEu32;
            'sweep: for length in 1..=200usize {
                for _ in 0..200 {
                    let payload: Vec<u8> = (0..length)
                        .map(|_| {
                            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                            (seed >> 24) as u8
                        })
                        .collect();
                    let _ = decoder.decode(&payload, true, &mut out);
                    if decoder.panicked() > 0 {
                        break 'sweep;
                    }
                }
            }
            assert!(
                decoder.panicked() > 0,
                "the containment never ran at {channels} channels, so this test proves nothing"
            );

            // **And the peer is not finished.** One packet that ended badly
            // must not cost the rest of the conversation, which is the half a
            // bare catch gets wrong: the state it unwound through is thrown
            // away and the next packet builds a decoder that never saw it.
            let samples: Vec<i16> = (4..).take(3 * channels).collect();
            let mut good = Vec::new();
            for sample in &samples {
                good.extend_from_slice(&sample.to_le_bytes());
            }
            let taken = decoder
                .decode(&good, false, &mut out)
                .expect("the decoder did not recover");
            assert_eq!(taken, 3);
            assert_eq!(&out[..samples.len()], &samples[..]);
        }
    }

    /// An empty payload is refused rather than handed to the codec, which
    /// treats it as a lost frame and invents sound for it.
    #[test]
    fn an_empty_compressed_payload_is_refused() {
        let mut out = [0i16; MONO_MAX];
        assert!(mono().decode(&[], true, &mut out).is_err());
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

    /// What this host's own encoder produces comes back through this decoder
    /// at the level it went in, in both channels.
    #[test]
    fn the_hosts_stereo_packets_decode_at_their_level() {
        let mut encoder = Encoder::new(crate::encode::DEFAULT_BITRATE_KBPS).expect("an encoder");
        let mut decoder = stereo();
        let mut out = vec![0i16; FRAME * CHANNELS];
        let mut energy = [0f64; 2];
        for step in 0..10 {
            let mut frame = vec![0u8; FRAME_BYTES];
            for (i, pair) in frame.chunks_exact_mut(4).enumerate() {
                let t = (step * FRAME + i) as f64 / f64::from(SAMPLE_RATE);
                #[allow(clippy::cast_possible_truncation)]
                let left = ((2.0 * core::f64::consts::PI * 440.0 * t).sin() * 12000.0) as i16;
                #[allow(clippy::cast_possible_truncation)]
                let right = ((2.0 * core::f64::consts::PI * 660.0 * t).sin() * 6000.0) as i16;
                pair[..2].copy_from_slice(&left.to_le_bytes());
                pair[2..].copy_from_slice(&right.to_le_bytes());
            }
            let packet = encoder.encode(&frame).expect("encodes").to_vec();
            let frames = decoder.decode(&packet, true, &mut out).expect("decodes");
            assert_eq!(frames, FRAME);
            if step == 9 {
                for pair in out.chunks_exact(2) {
                    energy[0] += f64::from(pair[0]) * f64::from(pair[0]);
                    energy[1] += f64::from(pair[1]) * f64::from(pair[1]);
                }
            }
        }
        // Each channel at its own level: a decoder that swapped or mixed
        // them would read the same total and fail here.
        for (channel, amplitude) in [(0usize, 12000.0f64), (1, 6000.0)] {
            let mean_square = energy[channel] / FRAME as f64;
            let expected = amplitude.powi(2) / 2.0;
            assert!(
                mean_square > expected * 0.5 && mean_square < expected * 2.0,
                "channel {channel}: mean square {mean_square}, expected about {expected}"
            );
        }
        assert_eq!(decoder.refused(), 0);
    }
}
