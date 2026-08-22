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
        match Capture::open(config, move |frame: &[u8]| {
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

    /// The device sound is read from, or empty when there is none.
    pub(crate) fn device(&self) -> String {
        self.capture
            .as_ref()
            .map_or_else(String::new, Capture::device)
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
