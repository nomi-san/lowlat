//! Everything a relay sends reaches this parser before anything else: its
//! answers, the datagrams it relays from peers, and its channel messages. They
//! are recognised by the relay's address alone, and anyone who can put that
//! address on a datagram can send one.
//!
//! Every accessor is exercised, not only the parse, because the offsets an
//! answer hands out are where a length mistake turns into a read past the end.
//! A relayed datagram is framed again the way this side sends one and must
//! read back as itself, which covers the framing in both directions.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::stun::TransactionId;
use lowlat_core::turn::{self, Inbound, Key};

fuzz_target!(|data: &[u8]| {
    let Ok(inbound) = turn::parse(data) else {
        return;
    };

    match inbound {
        Inbound::Response(response) => {
            let _ = response.method();
            let _ = response.transaction_id();
            let _ = response.is_success();
            let _ = response.error_code();
            let _ = response.challenge();
            let _ = response.relayed_address();
            let _ = response.lifetime_s();
            let _ = response.is_authenticated();
            let _ = response.verify(&Key::long_term("user", b"realm", "password"));
        }
        Inbound::Indication { peer, data: payload } => {
            let head = turn::indication_header_len(peer);
            let mut out = vec![0u8; head + payload.len() + 3];
            out[head..head + payload.len()].copy_from_slice(payload);
            let len = turn::wrap_indication(&mut out, TransactionId([0x5A; 12]), peer, payload.len())
                .expect("a relayed datagram must frame again");
            // A relay delivers what a client sends in the same layout, under
            // the data method rather than the send method.
            out[1] = 0x17;
            assert_eq!(
                turn::parse(&out[..len]),
                Ok(Inbound::Indication { peer, data: payload }),
                "a relayed datagram framed again did not read back as itself"
            );
        }
        Inbound::Channel { number, data: payload } => {
            // A relay may deliver on a number outside the ones a client binds;
            // this side never frames one.
            let mut out = vec![0u8; turn::CHANNEL_HEADER_LEN + payload.len()];
            out[turn::CHANNEL_HEADER_LEN..].copy_from_slice(payload);
            match turn::wrap_channel(&mut out, number, payload.len()) {
                Ok(len) => assert_eq!(
                    turn::parse(&out[..len]),
                    Ok(Inbound::Channel { number, data: payload }),
                    "channel data framed again did not read back as itself"
                ),
                Err(_) => assert!(
                    !(turn::FIRST_CHANNEL..=turn::LAST_CHANNEL).contains(&number),
                    "channel data on a bindable number would not frame again"
                ),
            }
        }
        _ => {}
    }
});
