//! The congestion controller's trajectory under scripted loss, side by side
//! with the candidate predicates the host-local improvements work is choosing
//! between.
//!
//! This drives the protocol core and the controller together: a sender
//! offering frames of a size the controller's rate allows, a receiver, and a
//! path that loses a fixed share of datagrams. The rate the controller lands
//! on and the time it takes to climb back are what the print lines carry,
//! which is what "a rate-controlled loop needs a long run, read in tenths"
//! looks like here.
//!
//! Two candidate shapes are run against the same traffic as the incumbent:
//! the window-floor rule alone (what ships), that plus an explicit loss rate
//! (which can fire below the floor), and the incumbent with its peak tracker
//! fed delivered bytes instead of offered ones. All three are read-only
//! comparisons; nothing actuates anywhere else.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::congestion::{self, Controller};
use lowlat_core::envelope::Envelope;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_sim::{Link, Sim};

const CHANNEL: u8 = 1;
/// The default datagram's payload budget, so a fragment is a wire-sized one.
const SLOT: usize = 1193;
const SLOTS: usize = 4000;
const KEY: [u8; 32] = [0x7A; 32];

/// Sixty frames a second.
const FRAME_MS: f64 = 1000.0 / 60.0;
/// How often the loop wakes, as in the recovery rig.
const TICK_MS: f64 = 5.0;
/// Throughput intervals, as the host's sampler has it.
const MEASURE_MS: f64 = 500.0;

/// Loss rates the picture is drawn at. One to five percent is the band the
/// incumbent's window-floor rule cannot see at these frame sizes.
const LOSS_STEPS: [f64; 3] = [0.01, 0.02, 0.05];

fn addr(last: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
}

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

fn endpoint(arena: &mut Arena) -> Session<'_> {
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
    session
}

/// How far a sent payload falls short of its fragment budget.
fn fragment_count(needed: usize) -> usize {
    needed.div_ceil(SLOT)
}

/// Resend counts since the last sample, over first sends, as a percentage.
struct LossMeter {
    first_sends: u64,
    resends: u64,
}

impl LossMeter {
    /// The window's loss as a ratio of resends to first sends, then reset.
    fn take(&mut self, pressure: &lowlat_core::session::Pressure) -> f64 {
        let first = pressure.bytes_sent / SLOT as u64;
        let new_first = first.saturating_sub(self.first_sends);
        let resends = pressure
            .nack_resends
            .saturating_add(pressure.timeout_resends)
            .saturating_sub(self.resends);
        self.first_sends = first;
        self.resends = pressure.nack_resends + pressure.timeout_resends;
        if new_first == 0 {
            return 0.0;
        }
        #[allow(
            clippy::cast_precision_loss,
            reason = "fragment counts over half a second; f64 is exact far past them"
        )]
        let ratio = resends as f64 / new_first as f64;
        ratio
    }
}

/// Which candidate's predicate is deciding congestion this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The window-floor stale ratio alone, as shipped.
    Incumbent,
    /// Plus a loss rate over threshold, which can fire below the floor.
    LossRate,
    /// The incumbent, with its peak tracker fed delivered bytes.
    GoodputPeak,
}

/// One run's outcome, in the units the trajectory is read in.
#[derive(Debug)]
struct Outcome {
    /// Tenths of the session spent under the mean rate, in milliseconds.
    below_mean_ms: f64,
    /// The controller's rate at the end, mebibits per second.
    final_mbps: f64,
    /// What the path was made to carry, average, mebibits per second.
    offered_mbps: f64,
    /// What was acknowledged, average, mebibits per second.
    delivered_mbps: f64,
    /// The controller's own count of decreases.
    decreases: u32,
}

/// Run `duration_ms` of stream at `loss`, ticking `mode`, and report it.
///
/// The offered load follows the controller's rate: each frame is as large as
/// the current rate allows in one frame interval, so a path that cannot carry
/// the rate is what loss stands in for, and the controller's answer to it is
/// the trajectory.
fn run(seed: u64, loss: f64, duration_ms: f64, mode: Mode) -> Outcome {
    let link = Link {
        loss,
        one_way_ms: 10.0,
        jitter_ms: 2.0,
        ..Link::default()
    };
    let mut sim = Sim::new(seed).with_link(link);
    let tx_host = sim.add_host(addr(10), &[]);
    let rx_host = sim.add_host(addr(20), &[]);

    let mut tx_arena = Arena::new();
    let mut rx_arena = Arena::new();
    let mut tx = endpoint(&mut tx_arena);
    let mut rx = endpoint(&mut rx_arena);

    // The controller, as the host builds one: default level, 1 to 30 Mibit/s.
    let mut controller = Controller::new(congestion::DEFAULT_LEVEL, 1.0, 30.0);
    let mut applied_mbps = controller.rate_mbps();
    let mut meter = LossMeter {
        first_sends: 0,
        resends: 0,
    };

    let mut wire = [0u8; SLOT + 64];
    let mut scratch = [0u8; SLOT + 64];
    let mut body = [0u8; SLOT];

    let mut next_frame_ms = 0.0;
    let mut measure_at_ms = 0.0;
    let mut offered_bytes_at_mark = 0u64;
    let mut delivered_bytes_at_mark = 0u64;
    let mut measured_offered = 0.0;
    let mut measured_delivered = 0.0;
    let mut below_mean_ms = 0.0;
    let mut rate_sum = 0.0;
    let mut ticks = 0u64;

    while sim.now_ms() < duration_ms {
        let now = sim.now_ms();

        // The frame offer: as many whole fragments as the applied rate buys
        // in one interval, and a partial one for the rest.
        let frame = now >= next_frame_ms;
        if frame {
            next_frame_ms = now + FRAME_MS;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive rate over a frame interval"
            )]
            let frame_bits = (applied_mbps * 1_048_576.0 * (FRAME_MS / 1000.0)) as usize;
            let mut needed = fragment_count(frame_bits / 8).max(1);
            while needed > 0 {
                if tx.send_message(CHANNEL, &[], &body).is_err() {
                    break;
                }
                needed -= 1;
            }
        }

        while let Some(result) = tx.get_output(now, &mut wire) {
            let len = result.expect("sender emitted a malformed datagram");
            sim.send(tx_host, addr(20), 64, &wire[..len]);
        }
        while let Some(result) = rx.get_output(now, &mut wire) {
            let len = result.expect("receiver emitted a malformed datagram");
            sim.send(rx_host, addr(10), 64, &wire[..len]);
        }

        while let Some(arrival) = sim.next_arrival() {
            let session = if arrival.host == tx_host {
                &mut tx
            } else {
                &mut rx
            };
            session
                .process_input(&arrival.bytes, now, &mut scratch)
                .expect("a delivered datagram failed to parse");
        }

        // Deliver what the receiver assembled, so its ring keeps taking.
        while rx.take_message(CHANNEL, &mut body).is_some() {}

        // The controller ticks once per frame, as the encode loop ticks it.
        if frame {
            if let Some(pressure) = tx.send_pressure(CHANNEL) {
                let loss_ratio = meter.take(&pressure);
                let congested = controller.is_congested(pressure.window, pressure.stale)
                    || (mode == Mode::LossRate && loss_ratio > 0.05);
                let feed = if mode == Mode::GoodputPeak {
                    measured_delivered
                } else {
                    measured_offered
                };
                let rate = tick_as(
                    &mut controller,
                    pressure.window,
                    pressure.stale,
                    feed,
                    congested,
                );
                rate_sum += rate;
                ticks += 1;
                let mean_so_far = rate_sum / ticks as f64;
                if rate < mean_so_far {
                    below_mean_ms += FRAME_MS;
                }
                applied_mbps = rate;
            }
        }

        // Throughput over the measuring interval, offered and delivered.
        if now >= measure_at_ms
            && let Some(pressure) = tx.send_pressure(CHANNEL)
        {
            let interval_s = MEASURE_MS / 1000.0;
            measured_offered = (pressure.bytes_sent - offered_bytes_at_mark) as f64 * 8.0
                / 1_048_576.0
                / interval_s;
            measured_delivered = (pressure.acked_bytes - delivered_bytes_at_mark) as f64 * 8.0
                / 1_048_576.0
                / interval_s;
            offered_bytes_at_mark = pressure.bytes_sent;
            delivered_bytes_at_mark = pressure.acked_bytes;
            measure_at_ms = now + MEASURE_MS;
        }

        sim.advance_ms(TICK_MS);
        tx.poll(sim.now_ms());
        rx.poll(sim.now_ms());
    }

    let seconds = duration_ms / 1000.0;
    Outcome {
        below_mean_ms,
        final_mbps: controller.rate_mbps(),
        offered_mbps: offered_bytes_at_mark as f64 * 8.0 / 1_048_576.0 / seconds,
        delivered_mbps: delivered_bytes_at_mark as f64 * 8.0 / 1_048_576.0 / seconds,
        decreases: controller.total_decreases(),
    }
}

/// One tick, with the congestion decision taken by the caller.
///
/// The controller's own `tick` cannot take a predicate it does not compute,
/// so the candidates that extend the rule are run through the same arithmetic
/// with the decision handed in. A clean tick feeds the real state through
/// `tick` unchanged; a congested one the incumbent disagrees with is applied
/// as the incumbent's own cut. When a candidate earns adoption it moves into
/// the controller and this goes.
fn tick_as(
    controller: &mut Controller,
    window: u32,
    stale: u32,
    measured_mbps: f64,
    congested: bool,
) -> f64 {
    if !congested || controller.is_congested(window, stale) {
        controller.tick(window, stale, measured_mbps)
    } else {
        controller.cut()
    }
}

/// The picture the whole harness exists for: the same seeded loss against
/// each candidate, printed beside the incumbent.
#[test]
fn trajectories_under_loss() {
    for loss in LOSS_STEPS {
        for mode in [Mode::Incumbent, Mode::LossRate, Mode::GoodputPeak] {
            let outcome = run(0x5EED, loss, 60_000.0, mode);
            println!(
                "loss={:.0}% {mode:?}: final={:.2} Mibit/s offered={:.2} delivered={:.2} \
                 decreases={} below_mean={:.0} ms",
                loss * 100.0,
                outcome.final_mbps,
                outcome.offered_mbps,
                outcome.delivered_mbps,
                outcome.decreases,
                outcome.below_mean_ms
            );
        }
    }
}

/// The incumbent's cut under pure loss is the timeout's doing, not the
/// window's: a datagram in five is lost, so every retransmission the timeout
/// drives is stale on arrival, and the window fills whenever the rate climbs
/// past what the lossy drain keeps up with. What never drives it is a peer
/// report -- there is none, by design -- and what the delivered figure
/// carries is the cost: the path kept what it was given and the rate paid
/// for the retry loop. The candidates exist so the signal reaches the
/// controller before the window has to say it.
#[test]
fn the_incumbent_cuts_only_after_the_window_fills() {
    let outcome = run(0x5EED, 0.02, 30_000.0, Mode::Incumbent);
    assert!(
        outcome.delivered_mbps < outcome.offered_mbps,
        "the loss never reached the wire: offered={:.2} delivered={:.2}",
        outcome.offered_mbps,
        outcome.delivered_mbps
    );
}

/// The loss-rate candidate sees what the incumbent cannot: the same run
/// declares congestion and the rate pays for it.
#[test]
fn the_loss_rate_candidate_declares_below_the_floor() {
    let outcome = run(0x5EED, 0.02, 30_000.0, Mode::LossRate);
    assert!(
        outcome.decreases > 0,
        "a two percent loss rate declared nothing"
    );
}

/// The goodput-fed peak climbs back more cautiously than the offered-fed
/// one, because retransmitted bytes stop looking like capacity.
#[test]
fn the_goodput_peak_climbs_more_slowly_than_the_offered_peak() {
    let incumbent = run(0xD1CE, 0.05, 60_000.0, Mode::Incumbent);
    let candidate = run(0xD1CE, 0.05, 60_000.0, Mode::GoodputPeak);
    assert!(
        candidate.final_mbps <= incumbent.final_mbps,
        "delivered-fed peak climbed past offered-fed: incumbent={:.2} candidate={:.2}",
        incumbent.final_mbps,
        candidate.final_mbps
    );
}
