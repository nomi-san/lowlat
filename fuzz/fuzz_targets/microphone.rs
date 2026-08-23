//! A guest's microphone: the wire parse, and the codec behind it.
//!
//! **Sequences rather than packets.** The decoder carries state between
//! packets and the panic this target exists to find is a property of that
//! state, not of any one payload: the same bytes handed to a fresh decoder
//! decode without complaint. So one input is chopped into a run of packets fed
//! to one decoder, which is the shape a guest's stream actually has.
//!
//! **A green run here does not say the codec is sound.** A panic inside it is
//! caught one layer down and turned into a refusal, which is the whole point
//! of that layer -- so this target can only find one that escapes. What the
//! codec does to itself shows up in production as the decoder's own count of
//! contained panics, and nowhere else.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_audio::microphone::Decoder;
use lowlat_core::microphone::{self, Encoding, Packet, SAMPLES_MAX};

fuzz_target!(|data: &[u8]| {
    // The wire half: a body straight off the control channel.
    if let Ok(Some(packet)) = microphone::parse(
        microphone::MICROPHONE_ARGUMENT,
        microphone::MICROPHONE_SELECTOR,
        data,
    ) {
        assert!(packet.payload.len() <= microphone::PAYLOAD_MAX);
    }

    // The codec half, as a stream. Lengths come from the input itself so the
    // fuzzer can steer how a run is cut up.
    let Ok(mut decoder) = Decoder::new() else {
        return;
    };
    let mut out = [0i16; SAMPLES_MAX];
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
        let _ = decoder.decode(
            &Packet {
                payload,
                encoding: Encoding::Compressed,
            },
            &mut out,
        );
    }
});
