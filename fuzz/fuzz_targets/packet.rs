//! Cleartext packet parsing, and the encoder fed back its own output.
//!
//! Re-encoding a parsed packet must reproduce the bytes exactly. A divergence
//! here is a wire bug that the corpus might not happen to cover.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::packet::{self, Packet};

fuzz_target!(|data: &[u8]| {
    let Ok(parsed) = packet::parse(data) else {
        return;
    };
    let mut out = [0u8; lowlat_core::MAX_CLEARTEXT];
    match parsed {
        Packet::Data(ref inner) => {
            if let Ok(written) = packet::encode_data(&mut out, inner) {
                assert_eq!(&out[..written], data, "data packet did not round trip");
            }
        }
        Packet::Ack(ref inner) => {
            if let Ok(written) = packet::encode_ack(&mut out, inner) {
                assert_eq!(
                    &out[..written],
                    &data[..written],
                    "acknowledgement did not round trip"
                );
            }
        }
    }
});
