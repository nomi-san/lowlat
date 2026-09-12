//! The browser pipe's record layer, fed what anyone who can reach the socket
//! can send it, in every state it passes through.
//!
//! The input is a script: a leading byte with its top bit set advances a real
//! peer one exchange, so the client walks from its first flight through the
//! handshake to an association; any other byte begins a datagram of that
//! length which is fed to the client as if it had arrived from the network.
//! The peer is a session in the server role built from the same code, which
//! is what makes the deeper states reachable at all. Nothing may panic, and
//! the only ways out are a fault and a liveness verdict.
#![no_main]

use libfuzzer_sys::fuzz_target;
use lowlat_core::endpoint::Media;
use lowlat_crypto::cert::Certificate;
use lowlat_net::web::{Role, WebSession};
use std::sync::OnceLock;

/// One identity per process: minting a certificate per input would make the
/// fuzzer measure key generation.
fn identities() -> &'static (Certificate, Certificate) {
    static IDENTITIES: OnceLock<(Certificate, Certificate)> = OnceLock::new();
    IDENTITIES.get_or_init(|| {
        (
            Certificate::generate().expect("a certificate"),
            Certificate::generate().expect("a certificate"),
        )
    })
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

    let mut wire = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut now = 0.0;
    let mut rest = data;
    while let Some((&head, tail)) = rest.split_first() {
        now += 10.0;
        if head & 0x80 != 0 {
            // One real exchange, both ways.
            client.poll(now);
            server.poll(now);
            while let Some(Ok(len)) = client.get_output(now, &mut wire) {
                let _ = server.process_input(&wire[..len], now, &mut scratch);
            }
            while let Some(Ok(len)) = server.get_output(now, &mut wire) {
                let _ = client.process_input(&wire[..len], now, &mut scratch);
            }
            rest = tail;
            continue;
        }
        let len = usize::from(head).min(tail.len());
        let (datagram, after) = tail.split_at(len);
        let _ = client.process_input(datagram, now, &mut scratch);
        let _ = client.poll(now);
        while let Some(result) = client.get_output(now, &mut wire) {
            let _ = result;
        }
        let _ = client.health(now);
        let _ = client.fault();
        let _ = client.send_pressure(1);
        rest = after;
    }
});
