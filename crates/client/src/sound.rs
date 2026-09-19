//! Sound: packets off the wire into a pool on the session's thread, decoded
//! on the application's thread when it asks.
//!
//! **No thread of its own and no window.** The receive loop copies each
//! packet once, off its ring into a slot, and stamps it; `acquire` takes the
//! next one in order and decodes it straight into the caller's buffer. What
//! paces playback is the application's device, which has the clock this
//! library does not (docs/10-client.md section 6). A full pool drops the
//! newest packet and counts it, which is a reader that is not calling.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use lowlat_common::pool::{self, Pool};
use lowlat_common::spsc::Ring;
use lowlat_core::audio::{self, AudioHeader, Codec};

use crate::driver::Telemetry;
use crate::report::Smoothed;

/// Packets waiting for the application: 640 ms at the host's 20 ms.
pub const SLOTS: usize = 32;
/// The longest sound packet: the uncompressed ceiling plus its header.
pub const PACKET_BYTES: usize = audio::PCM_PAYLOAD_MAX + audio::AUDIO_HEADER_LEN;
/// The most frames one packet may decode to: the uncompressed ceiling as
/// stereo, which also holds the codec's longest frame.
pub const FRAMES_MAX: usize = audio::PCM_PAYLOAD_MAX / 4;
/// Every host sends this; nothing else is decoded.
const CHANNELS: usize = 2;

/// Received sound packets, on their way to the application.
///
/// One producer (the session's thread) and one consumer. Shaped as the
/// access units are: the pool holds the bytes, the ring carries slot
/// indices, the word wakes the consumer.
#[derive(Debug, Clone)]
pub struct Packets {
    pool: Arc<Pool>,
    ring: Arc<Ring<u32, SLOTS>>,
    word: Arc<AtomicU32>,
}

impl Default for Packets {
    fn default() -> Self {
        Self::new()
    }
}

impl Packets {
    pub fn new() -> Self {
        Self {
            pool: Arc::new(Pool::new(SLOTS, PACKET_BYTES)),
            ring: Arc::new(Ring::new()),
            word: Arc::new(AtomicU32::new(0)),
        }
    }

    /// A slot to write the next packet into, or `None` while every one is
    /// still held: the producer's.
    pub(crate) fn writer(&self) -> Option<pool::Writer<'_>> {
        self.pool.acquire()
    }

    /// Hand a filled slot over, stamped with when it arrived; `false` when
    /// the ring refused, which cannot happen while it is as deep as the pool.
    pub(crate) fn publish(&self, writer: pool::Writer<'_>, arrived_ms: u32) -> bool {
        if writer.publish(arrived_ms, &[&self.ring]) == 0 {
            return false;
        }
        self.word.fetch_add(1, Ordering::Release);
        lowlat_common::wait::notify_one(&self.word);
        true
    }

    /// Wait up to `timeout` for a packet to be handed over, or for a wake.
    /// The consumer's; it rechecks after.
    pub fn wait(&self, timeout: Duration) {
        let word = self.word.load(Ordering::Acquire);
        if self.ring.is_empty() {
            lowlat_common::wait::wait(&self.word, word, timeout);
        }
    }

    /// Wake the consumer without a packet: teardown.
    pub fn wake(&self) {
        self.word.fetch_add(1, Ordering::Release);
        lowlat_common::wait::notify_all(&self.word);
    }

    /// The next packet, in order, or `None` while nothing waits.
    fn take(&self) -> Option<Packet<'_>> {
        let index = self.ring.pop()?;
        let frame = self.pool.claim(index)?;
        Some(Packet { frame })
    }

    /// Packets handed over and not yet taken.
    pub fn queued(&self) -> usize {
        self.ring.len()
    }
}

/// One packet, header included, as it came off the wire.
#[derive(Debug)]
struct Packet<'a> {
    frame: pool::Frame<'a>,
}

impl Packet<'_> {
    fn bytes(&self) -> &[u8] {
        self.frame.bytes()
    }

    /// When the receive loop took it, on its clock, in whole milliseconds.
    fn arrived_ms(&self) -> u32 {
        self.frame.tag()
    }
}

/// What one acquire handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Acquired {
    /// Frames (samples per channel) written.
    pub frames: usize,
    /// How long the packet waited between the wire and this call.
    pub age_ms: u32,
}

/// The caller's buffer holds fewer frames than the packet; carries how many
/// it needs. The packet is kept for the next call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooSmall(pub usize);

/// The consumer: one decoder, built from the stream's own header.
pub struct Sound {
    packets: Packets,
    decoding: Decoding,
}

/// The decoder and what it writes into, apart from the packets so a packet
/// held out of the pool and the decoder can be borrowed together.
struct Decoding {
    telemetry: Arc<Telemetry>,
    decoder: Option<lowlat_audio::Decoder>,
    /// What the decoder was built for; a change rebuilds it.
    built_for: Option<(u32, Codec, u8)>,
    /// A stream this cannot decode was named once already.
    unsupported_said: bool,
    /// Where a packet decodes to before it is copied out, so a buffer too
    /// small to take it costs nothing but the next call.
    pcm: Vec<i16>,
    /// Frames decoded and not yet handed over, at the front of `pcm`.
    held: Option<Acquired>,
    decoded: u64,
    refused: u32,
    /// The decode time per packet, smoothed: what the host is told.
    reported: Smoothed,
}

impl std::fmt::Debug for Sound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sound")
            .field("built_for", &self.decoding.built_for)
            .field("decoded", &self.decoding.decoded)
            .field("refused", &self.decoding.refused)
            .finish()
    }
}

impl Sound {
    pub fn new(packets: Packets, telemetry: Arc<Telemetry>) -> Self {
        Self {
            packets,
            decoding: Decoding {
                telemetry,
                decoder: None,
                built_for: None,
                unsupported_said: false,
                pcm: vec![0; FRAMES_MAX * CHANNELS],
                held: None,
                decoded: 0,
                refused: 0,
                reported: Smoothed::default(),
            },
        }
    }

    pub fn packets(&self) -> &Packets {
        &self.packets
    }

    /// Drop everything waiting, decoded or not: a session ended.
    pub fn clear(&mut self) {
        while self.packets.take().is_some() {}
        self.decoding.held = None;
    }

    /// The next packet, decoded into `out` as interleaved stereo, waiting up
    /// to `timeout` for one. `now_ms` is on the receive loop's clock, for
    /// the age. `Ok(None)` when none came in time; a packet that cannot be
    /// decoded is counted and the wait goes on.
    pub fn acquire(
        &mut self,
        now_ms: f64,
        timeout: Duration,
        out: &mut [i16],
    ) -> Result<Option<Acquired>, TooSmall> {
        if let Some(held) = self.decoding.held {
            return self.decoding.hand_over(held, out).map(Some);
        }
        let began = lowlat_common::clock::Time::now();
        let mut remaining = timeout;
        loop {
            let word = self.packets.word.load(Ordering::Acquire);
            if let Some(packet) = self.packets.take() {
                if let Some(acquired) = self.decoding.decode(&packet, now_ms) {
                    drop(packet);
                    return self.decoding.hand_over(acquired, out).map(Some);
                }
                continue;
            }
            if remaining.is_zero() {
                return Ok(None);
            }
            lowlat_common::wait::wait(&self.packets.word, word, remaining);
            let elapsed = lowlat_common::clock::elapsed_ms(began);
            let total = timeout.as_secs_f64() * 1000.0;
            remaining = if elapsed >= total {
                Duration::ZERO
            } else {
                Duration::from_secs_f64((total - elapsed) / 1000.0)
            };
        }
    }
}

impl Decoding {
    /// From the staging buffer into the caller's, or say how much it needs.
    fn hand_over(&mut self, acquired: Acquired, out: &mut [i16]) -> Result<Acquired, TooSmall> {
        let samples = acquired.frames * CHANNELS;
        let (Some(target), Some(source)) = (out.get_mut(..samples), self.pcm.get(..samples)) else {
            self.held = Some(acquired);
            return Err(TooSmall(acquired.frames));
        };
        target.copy_from_slice(source);
        self.held = None;
        Ok(acquired)
    }

    /// One packet into the staging buffer, or `None` when it was refused.
    fn decode(&mut self, packet: &Packet<'_>, now_ms: f64) -> Option<Acquired> {
        let bytes = packet.bytes();
        let Ok(header) = audio::parse(bytes) else {
            return self.refuse();
        };
        let payload = bytes.get(audio::AUDIO_HEADER_LEN..)?;
        let key = (header.mask, header.codec, header.channels);
        if self.built_for != Some(key) {
            // Remembered whether or not a decoder came of it, so a stream
            // this does not decode is refused per packet and not rebuilt
            // per packet, and the next header that differs is tried.
            let built = self.build(&header);
            self.built_for = Some(key);
            if !built {
                return self.refuse();
            }
        }
        // Absent for a stream this does not decode: every packet of it is
        // refused and counted.
        let Some(decoder) = self.decoder.as_mut() else {
            return self.refuse();
        };
        let compressed = header.codec == Codec::Opus;
        let began = lowlat_common::clock::Time::now();
        let Ok(frames) = decoder.decode(payload, compressed, &mut self.pcm) else {
            return self.refuse();
        };
        let reported = self.reported.push(lowlat_common::clock::elapsed_ms(began));
        self.telemetry
            .audio_reported_us
            .store(reported, Ordering::Relaxed);
        self.decoded = self.decoded.saturating_add(1);
        self.telemetry
            .audio_decoded
            .store(self.decoded, Ordering::Relaxed);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "milliseconds since the loop's epoch, wrapping as the stamp does"
        )]
        let now = now_ms.max(0.0) as u32;
        let age_ms = now.wrapping_sub(packet.arrived_ms());
        self.telemetry.audio_age_ms.store(age_ms, Ordering::Relaxed);
        Some(Acquired { frames, age_ms })
    }

    /// A decoder for what the header describes, or none for a stream this
    /// does not decode: anything but stereo at the protocol's rate.
    fn build(&mut self, header: &AudioHeader) -> bool {
        self.decoder = None;
        self.telemetry.audio_codec.store(0, Ordering::Relaxed);
        if usize::from(header.channels) != CHANNELS || header.rate != audio::SAMPLE_RATE {
            if !self.unsupported_said {
                self.unsupported_said = true;
                lowlat_common::log_warn!(
                    "client: sound not decoded, channels={} rate={} mask={}",
                    header.channels,
                    header.rate,
                    header.mask
                );
            }
            return false;
        }
        let Ok(decoder) = lowlat_audio::Decoder::new(CHANNELS, FRAMES_MAX) else {
            lowlat_common::log_warn!("client: no sound decoder");
            return false;
        };
        lowlat_common::log_info!(
            "client: sound decoder built, codec={:?} mask={} channels={} rebuild={}",
            header.codec,
            header.mask,
            header.channels,
            self.built_for.is_some()
        );
        self.decoder = Some(decoder);
        self.telemetry
            .audio_codec
            .store(u32::from(header.codec as u8), Ordering::Relaxed);
        true
    }

    fn refuse(&mut self) -> Option<Acquired> {
        self.refused = self.refused.saturating_add(1);
        self.telemetry
            .audio_refused
            .store(self.refused, Ordering::Relaxed);
        if self.refused == 1 {
            lowlat_common::log_warn!("client: a sound packet was refused");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo_raw(samples: u32, first: i16) -> Vec<u8> {
        let mut packet = vec![0u8; audio::AUDIO_HEADER_LEN];
        audio::encode(&mut packet, &AudioHeader::stereo(samples, Codec::Pcm)).unwrap();
        for i in 0..samples * 2 {
            packet.extend_from_slice(&(first + i as i16).to_le_bytes());
        }
        packet
    }

    fn push(packets: &Packets, bytes: &[u8], arrived_ms: u32) -> bool {
        let Some(mut writer) = packets.writer() else {
            return false;
        };
        assert!(writer.fill(bytes));
        packets.publish(writer, arrived_ms)
    }

    fn sound() -> (Sound, Packets) {
        let packets = Packets::new();
        (
            Sound::new(packets.clone(), Arc::new(Telemetry::default())),
            packets,
        )
    }

    /// Uncompressed packets come out as they went in, in order, with the
    /// age the stamps say.
    #[test]
    fn raw_packets_come_out_in_order_with_their_age() {
        let (mut sound, packets) = sound();
        assert!(push(&packets, &stereo_raw(4, 100), 10));
        assert!(push(&packets, &stereo_raw(2, 200), 12));
        let mut out = [0i16; 8];
        let first = sound
            .acquire(15.0, Duration::ZERO, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(
            first,
            Acquired {
                frames: 4,
                age_ms: 5
            }
        );
        assert_eq!(out, [100, 101, 102, 103, 104, 105, 106, 107]);
        let second = sound
            .acquire(15.0, Duration::ZERO, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(
            second,
            Acquired {
                frames: 2,
                age_ms: 3
            }
        );
        assert_eq!(&out[..4], &[200, 201, 202, 203]);
        assert_eq!(sound.acquire(15.0, Duration::ZERO, &mut out), Ok(None));
    }

    /// A buffer too small is told how much it needs, and the packet waits
    /// for the next call rather than being lost.
    #[test]
    fn a_small_buffer_is_told_the_need_and_the_packet_is_kept() {
        let (mut sound, packets) = sound();
        assert!(push(&packets, &stereo_raw(4, 1), 0));
        let mut small = [0i16; 6];
        assert_eq!(
            sound.acquire(0.0, Duration::ZERO, &mut small),
            Err(TooSmall(4))
        );
        assert_eq!(
            sound.acquire(0.0, Duration::ZERO, &mut small),
            Err(TooSmall(4))
        );
        let mut out = [0i16; 8];
        let taken = sound
            .acquire(0.0, Duration::ZERO, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(taken.frames, 4);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    /// The pool is as deep as it is; the thirty-third packet has nowhere to
    /// go and the producer is told so.
    #[test]
    fn a_full_pool_refuses_the_newest() {
        let (mut sound, packets) = sound();
        for i in 0..SLOTS {
            assert!(push(&packets, &stereo_raw(1, i as i16), 0), "slot {i}");
        }
        assert!(packets.writer().is_none());
        assert_eq!(packets.queued(), SLOTS);
        let mut out = [0i16; 2];
        assert!(
            sound
                .acquire(0.0, Duration::ZERO, &mut out)
                .unwrap()
                .is_some()
        );
        assert_eq!(out, [0, 1]);
        assert!(push(&packets, &stereo_raw(1, 99), 0));
    }

    /// A header the decoder was not built for rebuilds it; one this cannot
    /// decode refuses the packet and is said once.
    #[test]
    fn the_header_builds_the_decoder_and_an_unsupported_one_refuses() {
        let (mut sound, packets) = sound();
        let mut out = [0i16; 8];
        assert!(push(&packets, &stereo_raw(2, 1), 0));
        assert!(
            sound
                .acquire(0.0, Duration::ZERO, &mut out)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            sound.decoding.built_for,
            Some((audio::STEREO_MASK, Codec::Pcm, 2))
        );

        let mut mono = vec![0u8; audio::AUDIO_HEADER_LEN];
        audio::encode(
            &mut mono,
            &AudioHeader {
                mask: 1,
                samples: 2,
                rate: audio::SAMPLE_RATE,
                codec: Codec::Pcm,
                channels: 1,
            },
        )
        .unwrap();
        mono.extend_from_slice(&[1, 0, 2, 0]);
        assert!(push(&packets, &mono, 0));
        assert!(push(&packets, &stereo_raw(2, 5), 0));
        // The mono packet is refused and the wait goes on to the stereo one.
        let taken = sound
            .acquire(0.0, Duration::ZERO, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(taken.frames, 2);
        assert_eq!(&out[..4], &[5, 6, 7, 8]);
        assert_eq!(sound.decoding.refused, 1);
        assert!(sound.decoding.decoder.is_some());
    }

    /// What this host's encoder produces decodes here at its level.
    #[test]
    fn compressed_packets_decode() {
        let (mut sound, packets) = sound();
        let mut encoder = lowlat_audio::Encoder::new(128).unwrap();
        let mut out = vec![0i16; FRAMES_MAX * CHANNELS];
        let mut energy = 0f64;
        for step in 0..10u32 {
            let mut frame = vec![0u8; lowlat_audio::FRAME_BYTES];
            for (i, pair) in frame.chunks_exact_mut(4).enumerate() {
                let t = (step as usize * lowlat_audio::FRAME + i) as f64 / 48000.0;
                let v = ((2.0 * core::f64::consts::PI * 440.0 * t).sin() * 12000.0) as i16;
                pair[..2].copy_from_slice(&v.to_le_bytes());
                pair[2..].copy_from_slice(&v.to_le_bytes());
            }
            let mut packet = vec![0u8; audio::AUDIO_HEADER_LEN];
            audio::encode(&mut packet, &AudioHeader::stereo(960, Codec::Opus)).unwrap();
            packet.extend_from_slice(encoder.encode(&frame).unwrap());
            assert!(push(&packets, &packet, step));
            let taken = sound
                .acquire(f64::from(step), Duration::ZERO, &mut out)
                .unwrap()
                .unwrap();
            assert_eq!(taken.frames, 960);
            if step == 9 {
                energy = out[..1920]
                    .iter()
                    .map(|&v| f64::from(v) * f64::from(v))
                    .sum();
            }
        }
        let mean_square = energy / 1920.0;
        let expected = 12000f64.powi(2) / 2.0;
        assert!(mean_square > expected * 0.5 && mean_square < expected * 2.0);
        assert_eq!(sound.decoding.refused, 0);
        assert_eq!(sound.decoding.decoded, 10);
    }
}
