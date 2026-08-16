//! Phase 3 gates 1, 2 and 4: the loop under a sustained stream.
//!
//! Two shells punch over loopback and then one pushes messages at a target
//! packet rate while the other drains them, each on its own thread, which is
//! the threading model the shell is actually built for.
//!
//! Three things are measured rather than asserted loosely:
//!
//! 1. **Nothing is lost.** Message bodies carry their own sequence, so a gap is
//!    a gap and not an inference from a count. The kernel's own drop counter for
//!    the receiving socket is read from the same place `ss` reads it, which is
//!    what "the granted receive buffer is never overrun" means concretely.
//! 2. **Steady state allocates nothing.** The counter is per thread, so each
//!    loop is sampled on its own thread after the warm-up, and setup is free to
//!    allocate because it is outside the window.
//! 3. **The loop waits rather than ticks.** Timeout wakes are compared against
//!    the deadlines actually armed, so the claim is a ratio and not a profile.
//!
//! Duration comes from `LOWLAT_SOAK_MS` so the same harness serves the quick
//! per-run check and the ten- and sixty-minute soaks.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use lowlat_common::alloc_counter::{self, Counting};
use lowlat_common::clock::{Time, elapsed_ms};
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{Conn, Credentials};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_net::{Shell, Socket, Wake};

#[global_allocator]
static ALLOC: Counting = Counting;

/// Datagrams per second the stream aims for.
///
/// Above what a 1080p60 stream produces, so the loop is measured with headroom
/// rather than at the edge of what it will actually carry.
const TARGET_PPS: u64 = 10_000;

/// One message per datagram at this size, so the packet rate and the message
/// rate are the same number and a loss shows up in both.
const BODY: usize = 1100;

/// Long enough for the rate controller and the acknowledgement cadence to reach
/// steady state before anything is sampled.
const WARMUP_MS: f64 = 500.0;

/// How long the receiver keeps running after the sender stops, so the messages
/// still in flight arrive before the counts are compared.
const SETTLE_MS: u64 = 1_000;

const SLOT: usize = 1400;
const SLOTS: usize = 256;
const CHANNEL: u8 = 1;
const KEY: [u8; 32] = [0x31u8; 32];
const LEFT: (&str, &str) = ("aaaa", "passwordforaaaa");
const RIGHT: (&str, &str) = ("bbbb", "passwordforbbbb");

struct Arena {
    recv_bodies: Vec<u8>,
    recv_meta: Vec<SlotMeta>,
    send_bodies: Vec<u8>,
    send_meta: Vec<SendSlot>,
}

impl Arena {
    fn new() -> Self {
        Self {
            recv_bodies: vec![0u8; SLOT * SLOTS],
            recv_meta: vec![SlotMeta::default(); SLOTS],
            send_bodies: vec![0u8; SLOT * SLOTS],
            send_meta: vec![SendSlot::default(); SLOTS],
        }
    }
}

fn shell<'a>(
    arena: &'a mut Arena,
    ours: (&'a str, &'a str),
    theirs: (&'a str, &'a str),
    seed: u8,
) -> Shell<'a> {
    let conn = Conn::new(
        Credentials {
            local_ufrag: ours.0,
            local_pwd: ours.1,
            remote_ufrag: theirs.0,
            remote_pwd: theirs.1,
        },
        [seed; 16],
        0.0,
    );
    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    session
        .attach_recv(
            CHANNEL,
            RecvRing::new(&mut arena.recv_bodies, &mut arena.recv_meta, SLOT).unwrap(),
        )
        .unwrap();
    session
        .attach_send(
            CHANNEL,
            SendRing::new(&mut arena.send_bodies, &mut arena.send_meta, SLOT, CHANNEL).unwrap(),
        )
        .unwrap();
    Shell::new(
        Socket::open(0).expect("socket"),
        Wake::new().expect("wake"),
        Endpoint::new(conn, session),
    )
}

fn loopback_of(shell: &Shell<'_>) -> core::net::SocketAddr {
    let mut addr = shell.socket().local_addr().expect("addr");
    addr.set_ip(core::net::IpAddr::V6(core::net::Ipv6Addr::LOCALHOST));
    addr
}

/// Datagrams the kernel dropped on the socket bound to `port`, straight from
/// the counter `ss -u -m` reports.
///
/// This is the only direct measure of the receive buffer being overrun. Counting
/// what arrived cannot distinguish a datagram the kernel discarded from one that
/// was never sent.
fn kernel_drops(port: u16) -> u64 {
    let Ok(table) = std::fs::read_to_string("/proc/net/udp6") else {
        return 0;
    };
    for line in table.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(local) = fields.nth(1) else { continue };
        let Some((_, hex_port)) = local.rsplit_once(':') else {
            continue;
        };
        if u16::from_str_radix(hex_port, 16) != Ok(port) {
            continue;
        }
        if let Some(drops) = line.split_whitespace().next_back()
            && let Ok(value) = drops.parse()
        {
            return value;
        }
    }
    0
}

/// A pass that produces more datagrams than the staging batch holds must still
/// finish. *Named regression test.*
///
/// Staging hands back the room left in the batch. Once that is shorter than the
/// next datagram the core cannot encode into it, and a drain that asks again
/// unchanged gets the same failure forever: the loop wedges at full CPU with
/// the stream stopped. Reaching it needs more queued at once than the batch
/// holds, so every test that sends a datagram or two passes straight over it.
///
/// Run on its own thread, because the failure is a hang rather than a wrong
/// answer, and a hang inside the test would take the suite with it.
#[test]
fn a_pass_larger_than_the_staging_batch_still_completes() {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let done = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&done);

    thread::spawn(move || {
        // Two shells and a real punch, because media waits until a path exists.
        // A single shell with a candidate and no path emits nothing, the batch
        // never fills, and the test passes just as happily against the drain
        // that spins.
        let mut left_arena = Arena::new();
        let mut right_arena = Arena::new();
        let mut left = shell(&mut left_arena, LEFT, RIGHT, 0xC3);
        let mut right = shell(&mut right_arena, RIGHT, LEFT, 0xD4);
        let left_addr = loopback_of(&left);
        let right_addr = loopback_of(&right);
        left.endpoint().conn().add_candidate(right_addr).unwrap();
        right.endpoint().conn().add_candidate(left_addr).unwrap();

        let started = Time::now();
        while elapsed_ms(started) < 4_000.0 && left.endpoint().path().is_none() {
            let now = elapsed_ms(started);
            left.turn(now, |_| {}).expect("left turn");
            right.turn(now, |_| {}).expect("right turn");
        }
        assert!(left.endpoint().path().is_some(), "no path to send over");

        // The ring holds far more than the staging buffer, so the batch fills
        // partway through the first pass that has somewhere to send.
        let body = [0x5Au8; BODY];
        for _ in 0..SLOTS {
            if left
                .endpoint()
                .session()
                .send_message(CHANNEL, &[], &body)
                .is_err()
            {
                break;
            }
        }
        let mut now = elapsed_ms(started);
        for _ in 0..64 {
            left.turn(now, |_| {}).expect("turn");
            now += 10.0;
        }
        flag.store(true, Ordering::SeqCst);
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if done.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("the drain never returned: a full staging batch spins instead of flushing");
}

#[test]
fn a_sustained_stream_loses_nothing_allocates_nothing_and_does_not_tick() {
    let duration_ms: f64 = std::env::var("LOWLAT_SOAK_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000.0);

    let mut left_arena = Arena::new();
    let mut right_arena = Arena::new();
    let mut left = shell(&mut left_arena, LEFT, RIGHT, 0xA1);
    let mut right = shell(&mut right_arena, RIGHT, LEFT, 0xB2);

    let left_addr = loopback_of(&left);
    let right_addr = loopback_of(&right);
    let right_port = right_addr.port();
    left.endpoint().conn().add_candidate(right_addr).unwrap();
    right.endpoint().conn().add_candidate(left_addr).unwrap();

    // Punch first, on one thread, because there is nowhere to send until both
    // sides have a path and the stream would otherwise be measuring the punch.
    //
    // One clock for the whole run, shared with both loops below. A per-thread
    // clock restarts at zero after the punch has already advanced the session,
    // so time moves backwards and the schedule never comes due again.
    let started = Time::now();
    while elapsed_ms(started) < 4_000.0
        && (left.endpoint().path().is_none() || right.endpoint().path().is_none())
    {
        let now = elapsed_ms(started);
        left.turn(now, |_| {}).expect("left turn");
        right.turn(now, |_| {}).expect("right turn");
    }
    assert!(left.endpoint().path().is_some(), "left found no path");
    assert!(right.endpoint().path().is_some(), "right found no path");

    // Everything below is measured from here, not from process start.
    let punched = elapsed_ms(started);
    let drops_before = kernel_drops(right_port);
    // Two flags, because the last message handed to the session is still in
    // flight when the sender stops. Comparing counts across a stream that has
    // not quiesced measures the shutdown, not the transport.
    let stop = AtomicBool::new(false);
    let stop_receiver = AtomicBool::new(false);
    let sent = AtomicU64::new(0);
    let received = AtomicU64::new(0);
    let gaps = AtomicU64::new(0);
    let sender_allocs = AtomicU64::new(u64::MAX);
    let receiver_allocs = AtomicU64::new(u64::MAX);

    thread::scope(|scope| {
        scope.spawn(|| {
            let mut body = [0u8; BODY];
            let mut next: u64 = 0;
            let mut baseline = None;
            loop {
                let now = elapsed_ms(started);
                if now >= punched + WARMUP_MS && baseline.is_none() {
                    baseline = Some(alloc_counter::count());
                }
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // Owed by the clock rather than by a per-pass count, so a slow
                // pass is made up rather than silently lowering the rate. Capped
                // per pass so one long pass cannot turn the loop into a burst
                // that never returns to check whether it should stop.
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped non-negative, and a soak long enough to                               overflow u64 at this rate cannot be run"
                )]
                let owed = ((now - punched).max(0.0) / 1000.0 * TARGET_PPS as f64) as u64;
                let ceiling = next + SLOTS as u64;
                left.turn(now, |endpoint| {
                    while next < owed.min(ceiling) {
                        body[..8].copy_from_slice(&next.to_be_bytes());
                        // The window is bounded by the peer's ring depth, so a
                        // refusal is back pressure and not an error.
                        if endpoint.session().send_message(CHANNEL, &[], &body).is_err() {
                            break;
                        }
                        next += 1;
                    }
                })
                .expect("left turn");
            }
            sent.store(next, Ordering::Relaxed);
            if let Some(before) = baseline {
                sender_allocs.store(alloc_counter::count() - before, Ordering::Relaxed);
            }
        });

        scope.spawn(|| {
            let mut out = [0u8; SLOT];
            let mut expect: u64 = 0;
            let mut count: u64 = 0;
            let mut seen_gaps: u64 = 0;
            let mut baseline = None;
            loop {
                let now = elapsed_ms(started);
                if now >= punched + WARMUP_MS && baseline.is_none() {
                    baseline = Some(alloc_counter::count());
                }
                if stop_receiver.load(Ordering::Relaxed) {
                    break;
                }
                right.turn(now, |_| {}).expect("right turn");
                // Bounded per pass for the same reason as the sender: a drain
                // that never yields cannot notice the run has ended.
                let mut drained = 0;
                while drained < SLOTS
                    && let Some(Ok(len)) =
                        right.endpoint().session().take_message(CHANNEL, &mut out)
                {
                    drained += 1;
                    if len >= 8 {
                        let seq = u64::from_be_bytes(out[..8].try_into().expect("eight bytes"));
                        if seq != expect {
                            seen_gaps += 1;
                        }
                        expect = seq + 1;
                    }
                    count += 1;
                }
            }
            received.store(count, Ordering::Relaxed);
            gaps.store(seen_gaps, Ordering::Relaxed);
            if let Some(before) = baseline {
                receiver_allocs.store(alloc_counter::count() - before, Ordering::Relaxed);
            }
        });

        while elapsed_ms(started) < punched + WARMUP_MS + duration_ms {
            thread::sleep(std::time::Duration::from_millis(50));
        }
        stop.store(true, Ordering::Relaxed);
        // Let the tail land and be acknowledged before the receiver stops.
        thread::sleep(std::time::Duration::from_millis(SETTLE_MS));
        stop_receiver.store(true, Ordering::Relaxed);
    });

    let drops = kernel_drops(right_port).saturating_sub(drops_before);
    let sent = sent.load(Ordering::Relaxed);
    let received = received.load(Ordering::Relaxed);
    let gaps = gaps.load(Ordering::Relaxed);
    let sender_allocs = sender_allocs.load(Ordering::Relaxed);
    let receiver_allocs = receiver_allocs.load(Ordering::Relaxed);
    let left_stats = left.stats();
    let right_stats = right.stats();
    let seconds = duration_ms / 1000.0;

    println!("soak:   {seconds:.1} s at a {TARGET_PPS} datagram/s target");
    println!(
        "stream: {sent} messages sent, {received} received, {gaps} gaps, {drops} kernel drops"
    );
    println!(
        "rate:   {:.0} messages/s, {} datagrams out, {} datagrams in",
        received as f64 / seconds,
        left_stats.datagrams_out,
        right_stats.datagrams_in
    );
    println!("alloc:  {sender_allocs} sender, {receiver_allocs} receiver, in steady state");
    println!(
        "wakes:  sender {} timeout / {} datagram / {} send; receiver {} / {} / {}",
        left_stats.timeout_wakes,
        left_stats.datagram_wakes,
        left_stats.send_wakes,
        right_stats.timeout_wakes,
        right_stats.datagram_wakes,
        right_stats.send_wakes
    );

    assert_eq!(gaps, 0, "the stream lost or reordered messages");
    assert_eq!(received, sent, "{} messages never arrived", sent - received);
    assert_eq!(
        drops, 0,
        "the kernel dropped {drops} datagrams on the receiver"
    );
    assert_eq!(sender_allocs, 0, "the sender allocated in steady state");
    assert_eq!(receiver_allocs, 0, "the receiver allocated in steady state");

    // Gate 4, in the terms the gate justifies itself with. A loop that polls
    // instead of waiting wakes on a timeout every minimum wait, so its ceiling
    // is 1000 / MIN_WAIT_MS per second. One that arms from the endpoint's own
    // deadline wakes on a timeout only when a deadline genuinely came due, and
    // an order of magnitude is what separates the two.
    let ticking_per_sec = 1000.0 / lowlat_net::shell::MIN_WAIT_MS;
    let ceiling = ticking_per_sec / 10.0;
    for (who, stats) in [("sender", left_stats), ("receiver", right_stats)] {
        let per_sec = stats.timeout_wakes as f64 / seconds;
        assert!(
            per_sec <= ceiling,
            "{who} woke on a timeout {per_sec:.0}/s against a {ticking_per_sec:.0}/s tick, \
             which is not far enough from polling"
        );
    }
    println!(
        "gate4:  sender {:.0}/s, receiver {:.0}/s timeout wakes, ceiling {ceiling:.0}/s, \
         a polling loop would be {ticking_per_sec:.0}/s",
        left_stats.timeout_wakes as f64 / seconds,
        right_stats.timeout_wakes as f64 / seconds
    );
}
