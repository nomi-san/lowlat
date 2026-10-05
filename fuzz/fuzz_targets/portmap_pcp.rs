//! What comes back from the gateway's port, from anything that can put the
//! gateway's address on a datagram, options and all. An answer is taken only
//! within the protocol's lengths, and a mapping only whole.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::pcp;

fuzz_target!(|data: &[u8]| {
    if let Ok(reply) = pcp::parse(data) {
        assert!(
            (24..=pcp::MAX_LEN).contains(&data.len()),
            "an answer of no possible length"
        );
        if reply.map.is_some() {
            assert!(data.len() >= 60, "a short mapping taken");
        }
    }
});
