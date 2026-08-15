//! Video headers and keyframe classification.
//!
//! Classification walks into the bitstream, so a truncated frame must be
//! rejected rather than read past.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::video::{self, Codec};

fuzz_target!(|data: &[u8]| {
    let _ = video::is_keyframe(data, Codec::H264);
    let _ = video::is_keyframe(data, Codec::H265);
    if let Ok(header) = video::parse(data) {
        let _ = header.display_dimensions();
        let mut out = [0u8; video::VIDEO_HEADER_LEN];
        let _ = video::encode(&mut out, &header);
    }
});
