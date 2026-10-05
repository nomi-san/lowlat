//! What comes back from the gateway's port, from anything that can put the
//! gateway's address on a datagram. An answer is taken only as long as its
//! kind.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::natpmp::{self, Reply};

fuzz_target!(|data: &[u8]| match natpmp::parse(data) {
    Ok(Reply::Address { .. }) => assert!(data.len() >= 12, "a short address taken"),
    Ok(Reply::Map { .. }) => assert!(data.len() >= 16, "a short mapping taken"),
    Ok(Reply::Refused { .. }) => assert!(data.len() >= 8, "a short refusal taken"),
    Err(_) => {}
});
