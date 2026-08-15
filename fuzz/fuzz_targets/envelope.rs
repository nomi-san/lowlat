//! Every byte of a datagram reaches this before anything else, from anyone.
//!
//! Authentication must fail cleanly on garbage rather than panicking, and the
//! output buffer is deliberately smaller than the largest legal datagram so a
//! length mistake shows up as a refusal rather than a write past the end.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::envelope::Envelope;

fuzz_target!(|data: &[u8]| {
    let envelope = Envelope::from_key(&[0x11u8; 32]).unwrap();
    let mut out = [0u8; lowlat_core::MAX_CLEARTEXT];
    let _ = envelope.open(data, &mut out);

    let legacy = Envelope::from_key(&[0x22u8; 16]).unwrap();
    let _ = legacy.open(data, &mut out);
});
