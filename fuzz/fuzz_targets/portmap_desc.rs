//! A gateway's description, from whatever answered a search with a location.
//! Every service taken is one of the known kinds at a path from the root.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::desc;
use lowlat_portmap::url::Url;

fuzz_target!(|data: &[u8]| {
    let location = Url::parse("http://192.168.1.1:5000/rootDesc.xml").expect("a fixed location");
    if let Ok(found) = desc::parse(data, &location) {
        for service in found.connections() {
            assert!(
                desc::CONNECTIONS.contains(&service.service_type),
                "an unknown service taken"
            );
            assert!(
                service.control.path.starts_with('/'),
                "a control path not from the root"
            );
        }
    }
});
