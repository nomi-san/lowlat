//! The encode path allocates nothing.
//!
//! Audio is a real-time path in the sense AGENTS section 4 means: fifty frames
//! a second, every second a session lasts. Building the encoder is free to
//! allocate; the region inside `assert_no_alloc` is what runs per frame.
//!
//! **This also covers the codec**, which is a dependency rather than our code.
//! A pure-Rust port that allocated per frame would be invisible in review and
//! obvious here.

use lowlat_audio::encode::{self, DEFAULT_BITRATE_KBPS};
use lowlat_audio::{Encoder, FRAME, FRAME_BYTES, SAMPLE_RATE};
use lowlat_common::alloc_counter::{self, Counting};

#[global_allocator]
static ALLOC: Counting = Counting;

/// Frames of a tone, as capture delivers them.
fn frames(count: usize) -> Vec<Vec<u8>> {
    (0..count)
        .map(|step| {
            let mut frame = Vec::with_capacity(FRAME_BYTES);
            for i in 0..FRAME {
                let t = (step * FRAME + i) as f64 / f64::from(SAMPLE_RATE);
                let value = (2.0 * std::f64::consts::PI * 440.0 * t).sin() * 12000.0;
                #[allow(clippy::cast_possible_truncation)]
                let sample = value as i16;
                frame.extend_from_slice(&sample.to_le_bytes());
                frame.extend_from_slice(&sample.to_le_bytes());
            }
            frame
        })
        .collect()
}

#[test]
fn encoding_a_frame_does_not_allocate() {
    let mut encoder = Encoder::new(DEFAULT_BITRATE_KBPS).expect("an encoder");
    let frames = frames(64);
    // One frame outside the assertion, so anything the codec sets up on its
    // first call is not counted against the steady state.
    let _ = encoder.encode(&frames[0]).expect("encodes");

    alloc_counter::assert_no_alloc(|| {
        for frame in &frames {
            let packet = encoder.encode(frame).expect("encodes");
            std::hint::black_box(packet.len());
        }
    });
}

#[test]
fn changing_the_bitrate_does_not_allocate() {
    let mut encoder = Encoder::new(DEFAULT_BITRATE_KBPS).expect("an encoder");
    let frames = frames(8);
    let _ = encoder.encode(&frames[0]).expect("encodes");

    alloc_counter::assert_no_alloc(|| {
        for (step, frame) in frames.iter().enumerate() {
            encoder.set_bitrate(if step % 2 == 0 { 64 } else { 192 });
            let packet = encoder.encode(frame).expect("encodes");
            std::hint::black_box(packet.len());
        }
    });
}

#[test]
fn deciding_a_frame_is_silent_does_not_allocate() {
    let quiet = vec![0u8; FRAME_BYTES];
    let loud = frames(1);
    alloc_counter::assert_no_alloc(|| {
        assert!(encode::is_silent(&quiet));
        assert!(!encode::is_silent(&loud[0]));
    });
}
