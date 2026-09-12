//! The browser pipe's association, fed what a peer that has completed the
//! handshake can send it: bytes carried as real application data by the
//! far side's record layer, so the association's parser sees them exactly
//! as it would from a hostile browser.
//!
//! The pair is brought up deterministically first, then the input is a
//! script: a leading byte with its top bit set advances a real exchange,
//! which is how the association's own packets flow; any other byte begins a
//! payload of that length which the peer wraps and sends as application
//! data. Messages the association reassembles are taken, so the reader's
//! side of the queue is exercised too.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::endpoint::Media;
use lowlat_crypto::cert::Certificate;
use lowlat_net::web::{Role, WebSession};
use std::sync::OnceLock;

fn identities() -> &'static (Certificate, Certificate) {
    static IDENTITIES: OnceLock<(Certificate, Certificate)> = OnceLock::new();
    IDENTITIES.get_or_init(|| {
        (
            Certificate::generate().expect("a certificate"),
            Certificate::generate().expect("a certificate"),
        )
    })
}

fn exchange(client: &mut WebSession, server: &mut WebSession, now: f64) {
    let mut wire = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
    client.poll(now);
    server.poll(now);
    while let Some(Ok(len)) = client.get_output(now, &mut wire) {
        let _ = server.process_input(&wire[..len], now, &mut scratch);
    }
    while let Some(Ok(len)) = server.get_output(now, &mut wire) {
        let _ = client.process_input(&wire[..len], now, &mut scratch);
    }
}

fuzz_target!(|data: &[u8]| {
    let (ours, theirs) = identities();
    let Ok(mut client) =
        WebSession::new_seeded(Role::Client, Some(*theirs.fingerprint()), ours, 1, 0.0, 7)
    else {
        return;
    };
    let Ok(mut server) =
        WebSession::new_seeded(Role::Server, Some(*ours.fingerprint()), theirs, 1, 0.0, 11)
    else {
        return;
    };
    client.path_ready(0.0);
    server.path_ready(0.0);
    let mut now = 0.0;
    for _ in 0..40 {
        now += 10.0;
        exchange(&mut client, &mut server, now);
        if client.is_up() && server.is_up() {
            break;
        }
    }
    if !(client.is_up() && server.is_up()) {
        return;
    }

    let mut out = vec![0u8; 1 << 16];
    let mut rest = data;
    while let Some((&head, tail)) = rest.split_first() {
        now += 10.0;
        if head & 0x80 != 0 {
            exchange(&mut client, &mut server, now);
            rest = tail;
        } else {
            let len = usize::from(head).min(tail.len());
            let (payload, after) = tail.split_at(len);
            if !payload.is_empty() {
                server.inject_application_data(payload);
            }
            exchange(&mut client, &mut server, now);
            rest = after;
        }
        for channel in 0..19u8 {
            while let Some(result) = client.take_message(channel, &mut out) {
                let _ = result;
            }
        }
        let _ = client.health(now);
        let _ = client.fault();
        let _ = client.send_pressure(1);
        let _ = client.recv_drops(0);
    }
});
