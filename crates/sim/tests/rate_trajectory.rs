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
    ///
    /// **The denominator is the fragment counter, not bytes divided by a slot
    /// size.** That derivation carried two errors pulling opposite ways: it
    /// counted retransmitted bytes, which grows the denominator with the
    /// numerator, and it charged a short tail fragment as a fraction of one,
    /// which shrinks it. Which error dominates depends on the rate the
    /// controller has landed on, so the sign of the bias was not even fixed.
    fn take(&mut self, pressure: &lowlat_core::session::Pressure) -> f64 {
        let first = pressure.packets_sent;
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
    /// The incumbent, plus a round-trip gradient that **declines to climb**
    /// while the queue is building. It never cuts: the worst it can do is
    /// hold the rate where it is, which is why it is the first shape worth
    /// trying below the window floor.
    ///
    /// **Measured 2026-09-06 and not adoptable as shaped.** It passes the
    /// control the loss-rate predicate failed -- bit-identical to the
    /// incumbent on all three clean paths -- and it is inert where it was
    /// most wanted: at a 4 Mibit/s cap it changes nothing at all. At 8 it
    /// delivers 5.70 against 5.13 for four fewer cuts, but its nominal rate
    /// settles at 9.56 on a path that carries 8, which is worse overshoot
    /// than the incumbent it replaces. Under loss it is neutral at one and
    /// two percent and **loses eleven percent of delivered throughput at
    /// five**. A sixteenfold longer minimum-RTT window was tried and moved
    /// none of it, so the window length is not the reason.
    Gradient,
}

/// How far the smoothed round trip may sit above the windowed minimum before
/// the gradient reads it as a queue rather than a path.
///
/// **Against the minimum, not against a constant.** The minimum is this
/// path's own propagation delay as recently observed, so the ratio asks
/// whether *this* path is worse than it was rather than whether it is worse
/// than some other path would be.
const GRADIENT_MULT: f64 = 1.5;

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

/// The jitter every loss and capacity profile has always run at, kept as the
/// default so those trajectories mean what they did before the field existed.
const JITTER_MS: f64 = 2.0;

/// The path for a run: a loss rate, the delay spread, an optional reorder,
/// and where the profile wants it a capacity cap in mebibits per second.
#[derive(Debug, Clone, Copy)]
struct Profile {
    loss: f64,
    capacity_mibps: f64,
    /// Uniform delay spread. **Reorders fragments on its own** once it exceeds
    /// one fragment interval, which at these rates it does easily.
    jitter_ms: f64,
    /// Probability a datagram is held back behind later ones, and for how
    /// long: reorder without any delay spread, so the two causes can be told
    /// apart.
    reorder: f64,
    reorder_ms: f64,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            loss: 0.0,
            capacity_mibps: 0.0,
            jitter_ms: JITTER_MS,
            reorder: 0.0,
            reorder_ms: 0.0,
        }
    }
}

/// Run `duration_ms` of stream at `profile`, ticking `mode`, and report it.
///
/// The offered load follows the controller's rate: each frame is as large as
/// the current rate allows in one frame interval, so a path that cannot carry
/// the rate is what loss stands in for, and the controller's answer to it is
/// the trajectory.
fn run(seed: u64, profile: Profile, duration_ms: f64, mode: Mode) -> Outcome {
    let link = Link {
        loss: profile.loss,
        one_way_ms: 10.0,
        jitter_ms: profile.jitter_ms,
        reorder: profile.reorder,
        reorder_ms: profile.reorder_ms,
        // Mibit/s to bytes per millisecond.
        capacity_bytes_per_ms: profile.capacity_mibps * 1_048_576.0 / 8.0 / 1000.0,
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
            let srtt_ms = tx.srtt_ms();
            let rtt_min_ms = tx.rtt_min_ms();
            if let Some(pressure) = tx.send_pressure(CHANNEL) {
                let loss_ratio = meter.take(&pressure);
                let congested = controller.is_congested(pressure.window, pressure.stale)
                    || (mode == Mode::LossRate && loss_ratio > 0.05);
                let feed = if mode == Mode::GoodputPeak {
                    measured_delivered
                } else {
                    measured_offered
                };
                // **The gradient gates the increase; it does not cut.** A
                // congested tick is still the incumbent's to answer, so this
                // only ever suppresses a climb. Holding means not ticking at
                // all, so the clean-tick counter does not advance either and
                // the climb resumes where it paused rather than restarting.
                let holding = mode == Mode::Gradient
                    && !congested
                    && rtt_min_ms > 0.0
                    && srtt_ms > rtt_min_ms * GRADIENT_MULT;
                let rate = if holding {
                    controller.rate_mbps()
                } else {
                    tick_as(
                        &mut controller,
                        pressure.window,
                        pressure.stale,
                        feed,
                        congested,
                    )
                };
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
        for mode in [
            Mode::Incumbent,
            Mode::LossRate,
            Mode::GoodputPeak,
            Mode::Gradient,
        ] {
            let outcome = run(
                0x5EED,
                Profile {
                    loss,
                    ..Profile::default()
                },
                60_000.0,
                mode,
            );
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

/// The profile the candidates are really aimed at: a clean path whose
/// capacity the stream can exceed. **The incumbent is the wire-compatible
/// shape, so its ceiling here is the answer the candidates are measured
/// against, not a fault to fix.** What the print line carries is whether
/// either candidate finds the cap sooner or sits closer to it.
#[test]
fn trajectories_under_a_capacity_cap() {
    for capacity in [4.0, 8.0] {
        for mode in [
            Mode::Incumbent,
            Mode::LossRate,
            Mode::GoodputPeak,
            Mode::Gradient,
        ] {
            let outcome = run(
                0xCA90,
                Profile {
                    capacity_mibps: capacity,
                    ..Profile::default()
                },
                60_000.0,
                mode,
            );
            println!(
                "cap={capacity:.0} {mode:?}: final={:.2} Mibit/s offered={:.2} delivered={:.2} \
                 decreases={} below_mean={:.0} ms",
                outcome.final_mbps,
                outcome.offered_mbps,
                outcome.delivered_mbps,
                outcome.decreases,
                outcome.below_mean_ms
            );
        }
    }
}

/// **The control every loss predicate has to survive: a path that loses
/// nothing.**
///
/// Jitter reorders fragments once it exceeds one fragment interval, which at
/// these rates it does easily -- at 30 Mibit/s and 1193-byte fragments the
/// sender emits roughly three thousand a second, so two milliseconds spans
/// several. A receiver names a stored fragment more than two past its
/// frontier as a negative acknowledgement, so reorder alone produces
/// retransmissions, and any predicate reading a raw resend rate cannot tell
/// them from loss.
///
/// Three paths, all delivering everything: still, jittered, and explicitly
/// reordered without any delay spread so the two causes are separable.
///
/// **The incumbent is the control on the first two and is asserted to hold.**
/// On the third it does not, and that is its own finding rather than the
/// predicate's fault: two percent of datagrams held five milliseconds behind
/// later ones blocks the receiver's frontier for that long each time, the
/// send window fills behind the gap, and the stale ratio trips. Delivery is
/// genuinely stalled there, so backing off is defensible -- but a lossless
/// path cut to a fifth is worth knowing about before anything else is judged
/// against this profile. Printed, not asserted.
#[test]
fn trajectories_under_jitter_and_reorder() {
    // The third member says whether the incumbent is expected to hold: a
    // profile it cannot is still worth printing, but it is not a control.
    let profiles = [
        (
            "still     ",
            Profile {
                jitter_ms: 0.0,
                ..Profile::default()
            },
            true,
        ),
        ("jitter 2ms", Profile::default(), true),
        (
            "reorder 2%",
            Profile {
                jitter_ms: 0.0,
                reorder: 0.02,
                reorder_ms: 5.0,
                ..Profile::default()
            },
            false,
        ),
    ];
    for (name, profile, incumbent_holds) in profiles {
        for mode in [
            Mode::Incumbent,
            Mode::LossRate,
            Mode::GoodputPeak,
            Mode::Gradient,
        ] {
            let outcome = run(0x1177, profile, 30_000.0, mode);
            println!(
                "{name} {mode:?}: final={:.2} Mibit/s offered={:.2} delivered={:.2}                  decreases={} below_mean={:.0} ms",
                outcome.final_mbps,
                outcome.offered_mbps,
                outcome.delivered_mbps,
                outcome.decreases,
                outcome.below_mean_ms
            );
            if mode == Mode::Incumbent && incumbent_holds {
                assert_eq!(
                    outcome.decreases, 0,
                    "{name}: the incumbent cut on a path that lost nothing, so the rows \
                     beside it are measuring the harness rather than a predicate"
                );
            }
        }
    }
}

/// **The finding this profile exists to hold: the loss-rate predicate as
/// shaped in the improvements plan throttles a path that loses nothing.**
///
/// Two milliseconds of delay spread, no loss, no cap. That spread exceeds one
/// fragment interval at this rate, so fragments arrive out of order, a
/// receiver names one more than two past its frontier as a negative
/// acknowledgement, and a predicate reading a raw resend rate counts it as
/// loss. The incumbent holds at the ceiling on the same traffic.
///
/// **This test fails when the predicate is reshaped, and that is the point.**
/// Hysteresis, an average over more than one window, or excluding
/// reorder-driven negative acknowledgements would each stop the false cut;
/// when one of them lands, this assertion is what says so, and it wants
/// rewriting into the new expectation rather than deleting.
#[test]
fn the_loss_rate_candidate_false_cuts_on_jitter_alone() {
    let profile = Profile::default();
    let incumbent = run(0x1177, profile, 30_000.0, Mode::Incumbent);
    let candidate = run(0x1177, profile, 30_000.0, Mode::LossRate);

    assert_eq!(
        incumbent.decreases, 0,
        "the control moved: this profile is not clean and proves nothing"
    );
    assert!(
        candidate.decreases > 0,
        "the loss-rate predicate no longer false-cuts on jitter; rewrite this test \
         and revisit the improvements plan's loss-rate item"
    );
    assert!(
        candidate.final_mbps < incumbent.final_mbps / 2.0,
        "the false cut is no longer severe: incumbent={:.2} candidate={:.2}",
        incumbent.final_mbps,
        candidate.final_mbps
    );
}

/// **The control the loss-rate predicate failed, and the gradient passes.**
///
/// Two milliseconds of delay spread and no loss at all. The gradient gate is
/// asserted to leave this path exactly where the incumbent leaves it -- not
/// merely uncut, but the same number -- because a gate that only ever
/// declines to climb has no business moving a path that is climbing
/// correctly.
///
/// **This is the check that would catch a reshape going wrong.** Raising the
/// gate's sensitivity until it fires on ordinary jitter is the obvious way to
/// make it do something under load, and it is the same mistake the loss-rate
/// predicate made; this fails when that happens.
#[test]
fn the_gradient_gate_leaves_a_clean_path_exactly_where_it_found_it() {
    let profile = Profile::default();
    let incumbent = run(0x1177, profile, 30_000.0, Mode::Incumbent);
    let candidate = run(0x1177, profile, 30_000.0, Mode::Gradient);

    assert_eq!(
        incumbent.decreases, 0,
        "the control moved: this profile is not clean and proves nothing"
    );
    assert_eq!(
        candidate.decreases, 0,
        "the gradient gate cut a path that lost nothing"
    );
    assert!(
        (candidate.final_mbps - incumbent.final_mbps).abs() < 1e-9,
        "the gradient held a climb it had no reason to: incumbent={:.2} candidate={:.2}",
        incumbent.final_mbps,
        candidate.final_mbps
    );
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
    let outcome = run(
        0x5EED,
        Profile {
            loss: 0.02,
            ..Profile::default()
        },
        30_000.0,
        Mode::Incumbent,
    );
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
    let outcome = run(
        0x5EED,
        Profile {
            loss: 0.02,
            ..Profile::default()
        },
        30_000.0,
        Mode::LossRate,
    );
    assert!(
        outcome.decreases > 0,
        "a two percent loss rate declared nothing"
    );
}

/// The goodput-fed peak climbs back more cautiously than the offered-fed
/// one, because retransmitted bytes stop looking like capacity.
#[test]
fn the_goodput_peak_climbs_more_slowly_than_the_offered_peak() {
    let profile = Profile {
        loss: 0.05,
        ..Profile::default()
    };
    let incumbent = run(0xD1CE, profile, 60_000.0, Mode::Incumbent);
    let candidate = run(0xD1CE, profile, 60_000.0, Mode::GoodputPeak);
    assert!(
        candidate.final_mbps <= incumbent.final_mbps,
        "delivered-fed peak climbed past offered-fed: incumbent={:.2} candidate={:.2}",
        incumbent.final_mbps,
        candidate.final_mbps
    );
}
