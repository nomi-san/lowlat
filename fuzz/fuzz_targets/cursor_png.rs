//! The pointer's picture: a PNG a host sent, inflated and unfiltered into
//! RGBA. Whatever it is, it is refused or decoded and never panics.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_client::cursor;

fuzz_target!(|data: &[u8]| {
    let mut out = Vec::new();
    if let Ok(decoded) = cursor::decode_png(data, &mut out) {
        assert_eq!(out.len(), (decoded.width * decoded.height * 4) as usize);
        assert!(decoded.width <= cursor::MAX_SIDE && decoded.height <= cursor::MAX_SIDE);
    }
});
