//! A guest's microphone: the wire parse, and the codec behind it -- which is
//! also the codec the client reads a host's sound through, so the same run is
//! fed to a stereo decoder too.
//!
//! **Sequences rather than packets.** The decoder carries state between
//! packets and the panic this target exists to find is a property of that
//! state, not of any one payload: the same bytes handed to a fresh decoder
//! decode without complaint. So one input is chopped into a run of packets fed
//! to one decoder, which is the shape a peer's stream actually has.
//!
//! **A green run here does not say the codec is sound.** A panic inside it is
//! caught one layer down and turned into a refusal, which is the whole point
//! of that layer -- so this target can only find one that escapes. What the
//! codec does to itself shows up in production as the decoder's own count of
//! contained panics, and nowhere else. The harness's own panic hook aborts
//! the process before any unwinding, which would report every contained
//! panic as a crash; it is replaced with a silent one, and a panic that does
//! escape still reaches the harness's catch and aborts there.
#![no_main]

use std::sync::Once;

use libfuzzer_sys::fuzz_target;
use lowlat_audio::Decoder;
use lowlat_core::microphone::{self, SAMPLES_MAX};

/// The client's shape: stereo, a packet up to the uncompressed ceiling.
const STEREO_FRAMES: usize = lowlat_core::audio::PCM_PAYLOAD_MAX / 4;

static HOOK: Once = Once::new();

fuzz_target!(|data: &[u8]| {
    HOOK.call_once(|| std::panic::set_hook(Box::new(|_| {})));

    // The wire half: a body straight off the control channel.
    if let Ok(Some(packet)) = microphone::parse(
        data.len() as u32,
        microphone::MICROPHONE_ARGUMENT,
        microphone::MICROPHONE_SELECTOR,
        data,
    ) {
        assert!(packet.payload.len() <= microphone::PAYLOAD_MAX);
    }

    // The codec half, as a stream. Lengths come from the input itself so the
    // fuzzer can steer how a run is cut up.
    let (Ok(mut mono), Ok(mut stereo)) = (
        Decoder::new(microphone::CHANNELS, SAMPLES_MAX),
        Decoder::new(2, STEREO_FRAMES),
    ) else {
        return;
    };
    let mut out = vec![0i16; STEREO_FRAMES * 2];
    let mut rest = data;
    while let Some((&head, tail)) = rest.split_first() {
        let take = usize::from(head).min(tail.len());
        let (payload, next) = tail.split_at(take);
        rest = next;
        if payload.is_empty() {
            continue;
        }
        // Whatever it does, it must return: a panic reaching the harness is
        // the finding.
        let _ = mono.decode(payload, true, &mut out[..SAMPLES_MAX]);
        let _ = stereo.decode(payload, true, &mut out);
    }
});
