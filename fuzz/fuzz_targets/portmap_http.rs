//! A response from whatever holds the gateway's address, arriving in reads cut
//! anywhere; the first byte sets where. What is read depends on the bytes
//! alone, so the response read whole and the response read in pieces are one
//! outcome, and no body passes its cap.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_portmap::Result;
use lowlat_portmap::http::{Reader, Response};

const CAP: usize = 512;

fn read(pieces: &[&[u8]]) -> Result<Response> {
    let mut reader = Reader::new(CAP);
    for piece in pieces {
        if let Some(response) = reader.push(piece)? {
            return Ok(response);
        }
    }
    reader.finish()
}

fuzz_target!(|data: &[u8]| {
    let Some((&step, wire)) = data.split_first() else {
        return;
    };
    let whole = read(&[wire]);
    let pieces: Vec<&[u8]> = wire.chunks(usize::from(step).max(1)).collect();
    assert_eq!(
        whole,
        read(&pieces),
        "where the reads were cut changed what was read"
    );
    if let Ok(response) = whole {
        assert!(response.body.len() <= CAP, "a body past its cap");
    }
});
