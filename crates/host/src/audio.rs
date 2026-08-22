//! Sound, from the desktop to every guest.
//!
//! **One capture, one encode, and a slot per encoding actually wanted.** A room
//! where nobody asked for the uncompressed form never produces it, and a room
//! where everybody did never runs the codec. The two are independent, so a
//! mixed room costs both and neither costs anything per guest.
//!
//! **The header is not here.** It is fifteen bytes a guest can write for
//! itself: the sample count is fixed and the codec is that guest's own choice,
//! so the pool holds payload and nothing else -- exactly as it does for
//! pictures, where the header is per guest too.
//!
//! This thread is the sound server's in the sense that matters: it wakes when a
//! frame arrives and does nothing in between, so its cadence is the sound
//! device's own and never a timer of ours ([05 §9.1](../../../docs/05-host.md)).

use std::sync::Arc;

use lowlat_audio::encode::{self, DEFAULT_BITRATE_KBPS};
use lowlat_audio::{Capture, Encoder, FRAME, FRAME_BYTES};
use lowlat_core::audio::{AUDIO_HEADER_LEN, AudioHeader, Codec};

use crate::stream::Shared;

/// Slots in the audio pool.
///
/// **Two encodings times a short queue.** A guest behind by more than this
/// loses packets, which for sound is the right answer: a late one is worth less
/// than the one behind it.
const POOL_SLOTS: usize = 8;

/// What a room's sound costs, and who it goes to.
pub(crate) struct Sound {
    capture: Option<Capture>,
}

impl core::fmt::Debug for Sound {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sound")
            .field("capturing", &self.capture.is_some())
            .finish()
    }
}

impl Sound {
    /// Start capturing and publishing to whoever is seated.
    ///
    /// **A failure here is not a failure of the session.** A host with no sound
    /// server, or one it may not reach, streams pictures and says so once;
    /// refusing to host over something no guest has asked for would be worse.
    pub(crate) fn start(shared: &Arc<Shared>, config: lowlat_audio::Config) -> Self {
        let mut encoder = match Encoder::new(DEFAULT_BITRATE_KBPS) {
            Ok(encoder) => encoder,
            Err(error) => {
                lowlat_common::log_warn!("audio: no encoder, sound is off, error={error}");
                return Self { capture: None };
            }
        };
        let owned = Arc::clone(shared);
        let mut kbps = shared.sound_kbps();
        encoder.set_bitrate(kbps);
        match Capture::open(config, move |frame: &[u8]| {
            // **Read every frame and applied when it moves.** A rate that
            // needed a new encoder would cost a discontinuity a listener hears,
            // and one latched at start would be a setting that does nothing.
            let wanted = owned.sound_kbps();
            if wanted != kbps {
                kbps = wanted;
                encoder.set_bitrate(kbps);
                lowlat_common::log_info!("audio: encoding at {kbps} kbit/s");
            }
            publish(&owned, &mut encoder, frame);
        }) {
            Ok(capture) => {
                lowlat_common::log_info!("audio: sound is on, device={}", capture.device());
                Self {
                    capture: Some(capture),
                }
            }
            Err(error) => {
                lowlat_common::log_warn!("audio: sound is off, error={error}");
                Self { capture: None }
            }
        }
    }
}

/// One captured frame, to every seat that wants it.
fn publish(shared: &Shared, encoder: &mut Encoder, frame: &[u8]) {
    let (compressed, uncompressed) = shared.audio_wanted();

    if compressed {
        // **Silence is sent compressed.** The codec collapses it to about a
        // hundredth of the rate on its own, and a peer whose buffer drains pays
        // for it audibly when sound returns.
        match encoder.encode(frame) {
            Ok(packet) => shared.publish_audio(false, packet),
            Err(error) => lowlat_common::log_warn!("audio: frame not encoded, error={error}"),
        }
    }
    // **Silence is not sent uncompressed.** It would cost the whole rate to
    // carry nothing, which is the entire reason this test exists.
    if uncompressed && !encode::is_silent(frame) {
        shared.publish_audio(true, frame);
    }
}

/// What one guest's sound costs on the wire, in the unit the rate controllers
/// are tuned in.
///
/// **Header and length prefix included**, because a packet is what the uplink
/// carries rather than a payload. The uncompressed form is the whole frame; the
/// compressed one is whatever rate the encoder was asked for, which is what it
/// averages by construction.
pub(crate) fn guest_mbps(raw: bool, compressed_kbps: u32) -> f64 {
    let overhead = AUDIO_HEADER_LEN + lowlat_core::message::LENGTH_PREFIX_LEN;
    // Packets a second, from the frame this host sends.
    let packets = f64::from(lowlat_audio::SAMPLE_RATE) / FRAME_PER_SECOND_DIVISOR;
    let framing = f64::from(u32::try_from(overhead * 8).unwrap_or(0)) * packets;
    let bits = if raw {
        f64::from(u32::try_from(FRAME_BYTES * 8).unwrap_or(0)) * packets + framing
    } else {
        f64::from(compressed_kbps) * 1000.0 + framing
    };
    bits / 1_048_576.0
}

/// Samples in one packet, as the divisor that turns a rate into a packet count.
const FRAME_PER_SECOND_DIVISOR: f64 = FRAME as f64;

/// The fifteen bytes that precede one packet of this host's sound.
///
/// **Built per guest rather than shared**, because the codec is that guest's
/// own choice and the sample count is the same for both. It is arithmetic over
/// a stack array, which is why nothing caches it.
pub(crate) fn header(raw: bool) -> [u8; AUDIO_HEADER_LEN] {
    let codec = if raw { Codec::Pcm } else { Codec::Opus };
    let samples = u32::try_from(FRAME).unwrap_or(0);
    let mut bytes = [0u8; AUDIO_HEADER_LEN];
    let _ = lowlat_core::audio::encode(&mut bytes, &AudioHeader::stereo(samples, codec));
    bytes
}

/// How the pool behind all of this is built.
///
/// One slot holds a payload of either kind, so it is sized by the larger: the
/// uncompressed frame, which is exactly what capture delivers.
pub(crate) fn pool() -> crate::frames::Pool {
    crate::frames::Pool::new(POOL_SLOTS, FRAME_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **What a guest's sound costs, in the unit the controllers are tuned
    /// in.** The uncompressed form is the number the whole decision rests on:
    /// if it is wrong, the picture is given back the wrong share.
    #[test]
    fn the_uncompressed_form_costs_what_the_wire_carries() {
        // 3840 bytes of samples plus nineteen of framing, fifty times a
        // second, in mebibits.
        let expected = ((FRAME_BYTES + AUDIO_HEADER_LEN + 4) * 8 * 50) as f64 / 1_048_576.0;
        let measured = guest_mbps(true, 128);
        assert!(
            (measured - expected).abs() < 1e-9,
            "measured {measured}, expected {expected}"
        );
        // Which is about a megabit and a half.
        assert!((1.4..1.6).contains(&measured), "{measured} Mibit/s");
    }

    /// The compressed form is the rate that was asked for, plus its framing.
    #[test]
    fn the_compressed_form_costs_the_rate_it_was_asked_for() {
        let measured = guest_mbps(false, 128);
        let payload = 128_000.0 / 1_048_576.0;
        assert!(measured > payload, "framing was not counted");
        assert!(
            measured < payload * 1.1,
            "framing dominated the rate: {measured}"
        );
    }

    /// **The two differ by an order of magnitude**, which is the whole reason
    /// the choice is a guest's to make and the cost is the host's to account
    /// for.
    #[test]
    fn uncompressed_costs_an_order_of_magnitude_more() {
        assert!(guest_mbps(true, 128) > guest_mbps(false, 128) * 10.0);
    }
}
