//! Control and input messages, which carry application-controlled bodies.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::control;

fuzz_target!(|data: &[u8]| {
    if let Ok(parsed) = control::parse(data) {
        let _ = control::op::name(parsed.opcode);
        let mut out = [0u8; control::CONTROL_HEADER_LEN];
        if control::encode_header(&mut out, &parsed).is_ok() {
            assert_eq!(&out[..], &data[..control::CONTROL_HEADER_LEN]);
        }
    }
});
