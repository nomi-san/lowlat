//! Phase 13: what the browser pipe allocates and costs, per datagram.
//!
//! The native transport allocates nothing on its data path and a test
//! asserts it. This pipe allocates, a message and a packet at a time, by
//! decision (docs/00-overview.md D13); what the decision bought is recorded
//! here as numbers rather than assumed. A pair of sessions under a fake
//! clock, no sockets, carries a stream shaped like a real one -- sixty
//! 30 KiB pictures a second, fifty sound packets a second, a control
//! message now and then -- and the counting allocator and a monotonic clock
//! watch each side's data-path calls.
//!
//! Ignored because the figures only mean something in release and on a quiet
//! machine:
//!
//! ```text
//! cargo test --release -p lowlat-net --test web_bench -- --ignored --nocapture
//! ```

// Fixtures build bytes from loop counters and percentiles from lengths; the
// truncating casts are the obvious ones.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::time::Instant;

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_core::endpoint::Media;
use lowlat_crypto::cert::Certificate;
use lowlat_net::web::{Role, WebSession};

#[global_allocator]
static ALLOC: Counting = Counting;

const SECONDS: f64 = 10.0;
const FRAME: usize = 30 * 1024;
const FRAME_INTERVAL_MS: f64 = 1000.0 / 60.0;
const AUDIO: usize = 160;
const AUDIO_INTERVAL_MS: f64 = 20.0;
const CONTROL_INTERVAL_MS: f64 = 2000.0;

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[test]
#[ignore = "a measurement, run in release by hand"]
fn the_browser_pipe_costs_this_much_per_datagram() {
    let ours = Certificate::generate().unwrap();
    let theirs = Certificate::generate().unwrap();
    let mut host =
        WebSession::new(Role::Client, Some(*theirs.fingerprint()), &ours, 1, 0.0).unwrap();
    let mut guest =
        WebSession::new(Role::Server, Some(*ours.fingerprint()), &theirs, 1, 0.0).unwrap();
    host.path_ready(0.0);
    guest.path_ready(0.0);

    let mut wire = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
    let mut out = vec![0u8; 1 << 20];
    let mut now = 0.0;

    // Bring the pair up before anything is measured.
    while !(host.is_up() && guest.is_up()) && now < 5_000.0 {
        host.poll(now);
        guest.poll(now);
        while let Some(Ok(len)) = host.get_output(now, &mut wire) {
            let _ = guest.process_input(&wire[..len], now, &mut scratch);
        }
        while let Some(Ok(len)) = guest.get_output(now, &mut wire) {
            let _ = host.process_input(&wire[..len], now, &mut scratch);
        }
        now += 1.0;
    }
    assert!(host.is_up() && guest.is_up(), "no association");

    let frame = vec![0x5Au8; FRAME];
    let audio = vec![0xA5u8; AUDIO];
    let mut next_frame = now;
    let mut next_audio = now;
    let mut next_control = now;
    let end = now + SECONDS * 1000.0;

    let mut host_allocs = 0u64;
    let mut host_datagrams = 0u64;
    let mut host_messages = 0u64;
    let mut guest_allocs = 0u64;
    let mut guest_datagrams = 0u64;
    let mut guest_messages = 0u64;
    let mut host_us: Vec<f64> = Vec::with_capacity(1 << 20);
    let mut guest_us: Vec<f64> = Vec::with_capacity(1 << 20);

    let mut acks: Vec<Vec<u8>> = Vec::new();
    while now < end {
        // The host's pass: take the acknowledgements the last pass produced
        // -- which is where the association decides what to send next and
        // the record layer wraps it -- queue what is due, drain the rest.
        let before = alloc_counter::count();
        let started = Instant::now();
        for ack in acks.drain(..) {
            let _ = host.process_input(&ack, now, &mut scratch);
        }
        host.poll(now);
        if now >= next_frame {
            next_frame += FRAME_INTERVAL_MS;
            if host.send_message(1, b"VIDEO-HDR!", &frame).is_ok() {
                host_messages += 1;
            }
        }
        if now >= next_audio {
            next_audio += AUDIO_INTERVAL_MS;
            if host.send_message(2, b"AUDIO-HEADER-15", &audio).is_ok() {
                host_messages += 1;
            }
        }
        if now >= next_control {
            next_control += CONTROL_INTERVAL_MS;
            if host.send_message(0, b"HDR-13-BYTES!", b"").is_ok() {
                host_messages += 1;
            }
        }
        let mut produced = 0u64;
        let mut inbound = Vec::new();
        while let Some(Ok(len)) = host.get_output(now, &mut wire) {
            inbound.push(wire[..len].to_vec());
            produced += 1;
        }
        let host_pass_us = started.elapsed().as_secs_f64() * 1e6;
        host_allocs += alloc_counter::count() - before;
        host_datagrams += produced;
        if produced > 0 {
            host_us.push(host_pass_us / produced as f64);
        }

        // The guest's pass: take what arrived, drain the acknowledgements.
        let before = alloc_counter::count();
        let started = Instant::now();
        guest.poll(now);
        for datagram in &inbound {
            let _ = guest.process_input(datagram, now, &mut scratch);
            guest_datagrams += 1;
        }
        for channel in [0u8, 1, 2] {
            while let Some(Ok(_)) = guest.take_message(channel, &mut out) {
                guest_messages += 1;
            }
        }
        while let Some(Ok(len)) = guest.get_output(now, &mut wire) {
            acks.push(wire[..len].to_vec());
        }
        let guest_pass_us = started.elapsed().as_secs_f64() * 1e6;
        guest_allocs += alloc_counter::count() - before;
        if !inbound.is_empty() {
            guest_us.push(guest_pass_us / inbound.len() as f64);
        }
        now += 1.0;
    }

    host_us.sort_by(|a, b| a.total_cmp(b));
    guest_us.sort_by(|a, b| a.total_cmp(b));
    let pressure = host.send_pressure(1).unwrap();
    println!(
        "web_bench: {SECONDS:.0} s, {host_messages} messages offered, {host_datagrams} datagrams out, \
         {guest_messages} messages taken, {guest_datagrams} datagrams in; {} bytes delivered on video",
        pressure.acked_bytes
    );
    println!(
        "web_bench: host allocations {host_allocs} = {:.2} per datagram, {:.1} per message",
        host_allocs as f64 / host_datagrams.max(1) as f64,
        host_allocs as f64 / host_messages.max(1) as f64
    );
    println!(
        "web_bench: guest allocations {guest_allocs} = {:.2} per datagram, {:.1} per message",
        guest_allocs as f64 / guest_datagrams.max(1) as f64,
        guest_allocs as f64 / guest_messages.max(1) as f64
    );
    println!(
        "web_bench: host us per datagram p50 {:.2} p95 {:.2} p99 {:.2}; guest p50 {:.2} p95 {:.2} p99 {:.2}",
        percentile(&host_us, 0.50),
        percentile(&host_us, 0.95),
        percentile(&host_us, 0.99),
        percentile(&guest_us, 0.50),
        percentile(&guest_us, 0.95),
        percentile(&guest_us, 0.99),
    );
    assert_eq!(guest_messages, host_messages, "not every message was taken");
}
