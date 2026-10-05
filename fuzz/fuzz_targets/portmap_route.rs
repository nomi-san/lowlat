//! The routing table's text, read for the default gateway. Whatever is taken
//! is a gateway that names an address.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::route;

fuzz_target!(|data: &[u8]| {
    let Ok(table) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(gateway) = route::default_gateway(table) {
        assert!(!gateway.is_unspecified(), "a gateway of no address taken");
    }
});
