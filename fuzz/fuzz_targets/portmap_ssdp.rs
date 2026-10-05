//! An answer to a search, from anything on the local network that hears one.
//! Whatever is taken names a location, trimmed.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::ssdp;

fuzz_target!(|data: &[u8]| {
    if let Ok(answer) = ssdp::parse(data) {
        assert!(
            !answer.location.is_empty(),
            "an answer taken without a location"
        );
        assert_eq!(
            answer.location,
            answer.location.trim(),
            "a location left untrimmed"
        );
    }
});
