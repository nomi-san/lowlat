//! The input driver parses what its one caller sends: a create request, and
//! every report written to a device it made (docs/11-drivers.md section 2.2).
//! Whatever a create request reads as must write back and read again as
//! itself, and no report is taken that its kind does not declare whole.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_hid_proto::Kind;
use lowlat_hid_proto::control::{Create, Version, Written};

fuzz_target!(|data: &[u8]| {
    if let Ok(create) = Create::decode(data) {
        let mut out = [0u8; Create::LEN];
        let len = create.encode(&mut out).expect("a create that was read writes back");
        assert_eq!(Create::decode(&out[..len]), Ok(create), "a create that does not read back");
    }
    if let Ok(written) = Written::decode(data) {
        let mut out = [0u8; Written::LEN];
        let len = written.encode(&mut out).expect("a reply that was read writes back");
        assert_eq!(Written::decode(&out[..len]), Ok(written));
    }
    let _ = Version::decode(data);
    for kind in Kind::ALL {
        if kind.accepts(data) {
            assert_eq!(
                kind.input_len(data[0]),
                Some(data.len()),
                "{kind:?} took a report it does not declare whole"
            );
        }
    }
});
