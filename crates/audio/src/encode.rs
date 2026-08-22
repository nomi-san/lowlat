//! Turning a captured frame into what a guest asked for.
//!
//! **Two codecs and one capture.** A guest that asked for uncompressed is sent
//! the frame exactly as it was read, because the wire's uncompressed form *is*
//! interleaved sixteen-bit samples; a guest that did not is sent the packet
//! this module produces. The room may hold both at once, so the choice belongs
//! where a packet is handed to a guest and not here.
//!
//! **Nothing on this path allocates.** The conversion buffer and the packet
//! buffer are allocated when the encoder is built and reused for the life of
//! the stream.

use lowlat_core::audio::Codec;
use opus_rs::{Application, OpusEncoder};

use crate::{CHANNELS, Error, FRAME, FRAME_BYTES, SAMPLE_RATE};

/// The largest packet the codec will produce for one frame. Its own ceiling is
/// smaller; the margin costs a few hundred bytes once.
const PACKET_MAX: usize = 1500;

/// What this host asks for when nothing says otherwise.
///
/// Stereo desktop sound rather than speech, at a rate where the codec is
/// transparent for music and still under one percent of a modest video budget.
pub const DEFAULT_BITRATE_KBPS: u32 = 128;

/// One stream's encoder.
pub struct Encoder {
    opus: OpusEncoder,
    /// The codec takes float samples; capture gives sixteen-bit ones, because
    /// that is what the uncompressed path puts on the wire. The conversion is
    /// one pass over 20 ms and is measured with the encode rather than beside
    /// it.
    floats: Vec<f32>,
    packet: Vec<u8>,
}

impl core::fmt::Debug for Encoder {
    /// The codec's own state is large and says nothing a log can use.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Encoder")
            .field("bitrate_bps", &self.opus.bitrate_bps)
            .finish()
    }
}

impl Encoder {
    /// Build an encoder for this host's format.
    pub fn new(bitrate_kbps: u32) -> Result<Self, Error> {
        let rate = i32::try_from(SAMPLE_RATE).map_err(|_| Error::Encode)?;
        let mut opus =
            OpusEncoder::new(rate, CHANNELS, Application::Audio).map_err(|_| Error::Encode)?;
        opus.bitrate_bps = bits_per_second(bitrate_kbps);
        Ok(Self {
            opus,
            floats: vec![0.0; FRAME * CHANNELS],
            packet: vec![0u8; PACKET_MAX],
        })
    }

    /// Change the rate without rebuilding anything.
    ///
    /// **Takes effect on the next frame**, which is the whole point: a bitrate
    /// that needed a new encoder would cost a discontinuity a listener hears.
    pub fn set_bitrate(&mut self, kbps: u32) {
        self.opus.bitrate_bps = bits_per_second(kbps);
    }

    /// The rate in use, in bits per second.
    pub fn bitrate(&self) -> i32 {
        self.opus.bitrate_bps
    }

    /// Encode one captured frame.
    ///
    /// `frame` is exactly [`FRAME_BYTES`] of interleaved sixteen-bit samples,
    /// which is what capture delivers.
    pub fn encode(&mut self, frame: &[u8]) -> Result<&[u8], Error> {
        if frame.len() != FRAME_BYTES {
            return Err(Error::Encode);
        }
        for (dst, pair) in self.floats.iter_mut().zip(frame.chunks_exact(2)) {
            let sample = i16::from_le_bytes([pair[0], pair[1]]);
            *dst = f32::from(sample) / 32768.0;
        }
        let len = self
            .opus
            .encode(&self.floats, FRAME, &mut self.packet)
            .map_err(|_| Error::Encode)?;
        self.packet.get(..len).ok_or(Error::Encode)
    }
}

/// A frame nobody would hear.
///
/// **Exactly zero, not quiet.** A monitor with nothing playing delivers
/// digital silence, and that is what may be skipped; a threshold would drop the
/// quiet passages of real audio, which is a fault a listener notices and cannot
/// describe.
pub fn is_silent(frame: &[u8]) -> bool {
    frame.iter().all(|&byte| byte == 0)
}

/// What a packet of this codec is, for the header that precedes it.
///
/// The uncompressed form is the captured frame itself, so there is nothing to
/// produce and nothing to copy.
pub const fn payload_of<'a>(codec: Codec, frame: &'a [u8], encoded: &'a [u8]) -> &'a [u8] {
    match codec {
        Codec::Pcm => frame,
        Codec::Opus => encoded,
    }
}

fn bits_per_second(kbps: u32) -> i32 {
    i32::try_from(kbps.saturating_mul(1000)).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of a tone, as capture would deliver it.
    fn tone(hz: f64, amplitude: f64, offset: usize) -> Vec<u8> {
        let mut frame = Vec::with_capacity(FRAME_BYTES);
        for i in 0..FRAME {
            let t = (offset + i) as f64 / f64::from(SAMPLE_RATE);
            let value = (2.0 * core::f64::consts::PI * hz * t).sin() * amplitude;
            #[allow(clippy::cast_possible_truncation)]
            let sample = value as i16;
            frame.extend_from_slice(&sample.to_le_bytes());
            frame.extend_from_slice(&sample.to_le_bytes());
        }
        frame
    }

    #[test]
    fn a_frame_becomes_a_packet_that_decodes_back_to_the_tone() {
        let mut encoder = Encoder::new(DEFAULT_BITRATE_KBPS).expect("an encoder");
        let mut decoder =
            opus_rs::OpusDecoder::new(i32::try_from(SAMPLE_RATE).expect("a rate"), CHANNELS)
                .expect("a decoder");
        let mut out = vec![0f32; FRAME * CHANNELS];

        // The codec needs a few frames before it settles, so the tone runs on
        // and the last frame is the one measured.
        let mut energy = 0f64;
        for step in 0..10 {
            let frame = tone(440.0, 12000.0, step * FRAME);
            let packet = encoder.encode(&frame).expect("encodes").to_vec();
            assert!(!packet.is_empty() && packet.len() < PACKET_MAX);
            let samples = decoder.decode(&packet, FRAME, &mut out).expect("decodes");
            assert_eq!(samples, FRAME);
            if step == 9 {
                energy = out.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
            }
        }
        // **The level, not merely the presence.** A tone at this amplitude is
        // 12000 of 32768, so its mean square is half the square of that, near
        // 0.067. Checking only that something came out passes with the
        // conversion scaled wrongly by a factor of 256, which clips every
        // sample to full scale and sounds like distortion rather than silence
        // -- that was tried and did pass.
        let mean_square = energy / (FRAME * CHANNELS) as f64;
        let expected = (12000.0f64 / 32768.0).powi(2) / 2.0;
        assert!(
            mean_square > expected * 0.5 && mean_square < expected * 2.0,
            "decoded mean square {mean_square}, expected about {expected}"
        );
    }

    #[test]
    fn a_frame_of_the_wrong_size_is_refused() {
        let mut encoder = Encoder::new(DEFAULT_BITRATE_KBPS).expect("an encoder");
        assert!(encoder.encode(&[0u8; FRAME_BYTES - 2]).is_err());
        assert!(encoder.encode(&[0u8; FRAME_BYTES + 2]).is_err());
        assert!(encoder.encode(&[0u8; FRAME_BYTES]).is_ok());
    }

    /// **The rate is a rate, not a hint.** Doubling it has to show up in what
    /// the packets cost, or nothing an operator sets means anything.
    #[test]
    fn the_bitrate_reaches_the_packets() {
        let frames: Vec<Vec<u8>> = (0..50)
            .map(|step| tone(440.0, 12000.0, step * FRAME))
            .collect();
        let measure = |kbps: u32| -> usize {
            let mut encoder = Encoder::new(kbps).expect("an encoder");
            frames
                .iter()
                .map(|frame| encoder.encode(frame).expect("encodes").len())
                .sum()
        };
        let low = measure(64);
        let high = measure(192);
        assert!(
            high > low * 2,
            "64 kbit/s produced {low} bytes and 192 produced {high}"
        );
    }

    /// **The setter, not the constructor.** The test above builds a new encoder
    /// per rate, so it passes with a setter that ignores its argument -- which
    /// was tried and did pass. A live bitrate change is the one an application
    /// makes mid-session, and it has to move the packets.
    #[test]
    fn the_bitrate_can_be_changed_on_a_running_encoder() {
        let frames: Vec<Vec<u8>> = (0..80)
            .map(|step| tone(440.0, 12000.0, step * FRAME))
            .collect();
        let mut encoder = Encoder::new(64).expect("an encoder");
        let run = |encoder: &mut Encoder, from: usize| -> usize {
            frames[from..from + 30]
                .iter()
                .map(|frame| encoder.encode(frame).expect("encodes").len())
                .sum()
        };
        // Ten frames for the rate to take hold, then thirty measured.
        let _ = run(&mut encoder, 0);
        let low = run(&mut encoder, 30);
        encoder.set_bitrate(192);
        let _ = run(&mut encoder, 0);
        let high = run(&mut encoder, 30);
        assert!(
            high > low * 2,
            "the same encoder produced {low} bytes at 64 kbit/s and {high} at 192"
        );
    }

    #[test]
    fn silence_is_exactly_silence() {
        assert!(is_silent(&[0u8; FRAME_BYTES]));
        // One sample at the smallest non-zero value is not silence: a
        // threshold here would cut the quiet parts of real audio.
        let mut nearly = vec![0u8; FRAME_BYTES];
        nearly[FRAME_BYTES - 2] = 1;
        assert!(!is_silent(&nearly));
        assert!(!is_silent(&tone(440.0, 12000.0, 0)));
    }

    #[test]
    fn the_uncompressed_payload_is_the_captured_frame() {
        let frame = tone(440.0, 12000.0, 0);
        let encoded = [1u8, 2, 3];
        assert!(core::ptr::eq(
            payload_of(Codec::Pcm, &frame, &encoded).as_ptr(),
            frame.as_ptr()
        ));
        assert_eq!(payload_of(Codec::Opus, &frame, &encoded), &encoded);
    }
}
