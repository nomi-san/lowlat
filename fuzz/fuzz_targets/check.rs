//! Connectivity checks arrive on the media socket from anyone who can reach it,
//! before any credential has been agreed and before the peer is known.
//!
//! Every accessor is exercised, not only the parse, because the offsets a
//! parsed message hands out are where a length mistake turns into a read past
//! the end. Verification runs on every input too: it walks the message with a
//! digest and must refuse garbage rather than panic on it.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::demux::{self, Datagram};
use lowlat_core::stun::Message;

fuzz_target!(|data: &[u8]| {
    // The classifier decides what ever reaches this parser at all, so run it
    // first and keep the two consistent with the real receive path.
    let classified = demux::classify(data);

    let Ok(message) = Message::parse(data) else {
        return;
    };

    // A parsed message must answer every question without panicking, whatever
    // the attribute area contains.
    let _ = message.method();
    let _ = message.transaction_id();
    let _ = message.is_authenticated();
    let _ = message.username();
    let _ = message.mapped_address();

    // Both a plausible password and an empty one: the empty case exercises the
    // key-length boundary in the digest.
    let _ = message.verify("password");
    let _ = message.verify("");

    // Anything that parses as a check must have classified as one, or the
    // receive path would hand this input to the record layer instead and the
    // fuzzing would be covering a path that cannot be reached.
    assert_eq!(
        classified,
        Datagram::Check,
        "a parseable check must classify as a check"
    );
});
