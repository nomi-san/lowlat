//! A control answer, read whatever its status: two bytes of status, then the
//! body, read as the answer to each action a mapping sends.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::soap;

fuzz_target!(|data: &[u8]| {
    let Some((status, body)) = data.split_first_chunk::<2>() else {
        return;
    };
    let status = u16::from_be_bytes(*status);
    for action in [
        "AddPortMapping",
        "DeletePortMapping",
        "GetSpecificPortMappingEntry",
        "GetExternalIPAddress",
        "GetStatusInfo",
    ] {
        let _ = soap::parse(action, status, body);
    }
});
