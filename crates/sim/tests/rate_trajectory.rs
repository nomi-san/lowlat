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

/// Sixty frames a second, which is what every profile runs at unless it says
/// otherwise.
///
/// **The controller's periods are counted in frames, not in time**, so this is
/// also what turns thirty clean ticks into half a second and sixty congested
/// ticks into one. A stream running at another rate keeps the tick counts and
/// gets different durations, which is the thing
/// `trajectories_at_a_low_frame_rate` measures.
const FRAME_FPS: f64 = 60.0;
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
    timeouts: u64,
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
    ///
    /// **The numerator is timeouts alone, not every resend.** The two causes
    /// mean different things and the ring already separates them. A reorder
    /// fires a negative acknowledgement -- the receiver names a fragment more
    /// than two past its frontier -- and no timeout follows, because the gap
    /// fills on its own. A loss fires both: the report if the receiver
    /// notices, and the timeout when nothing arrives. Summing them made the
    /// predicate count reorder as loss, which is what threw away a clean
    /// thirty-megabit path on two milliseconds of delay spread.
    fn take(&mut self, pressure: &lowlat_core::session::Pressure) -> f64 {
        let first = pressure.packets_sent;
        let new_first = first.saturating_sub(self.first_sends);
        let resends = pressure.timeout_resends.saturating_sub(self.timeouts);
        self.first_sends = first;
        self.timeouts = pressure.timeout_resends;
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
    /// The timeout-only loss rate, answered gently instead of with a cut.
    ///
    /// **The response, not the detector.** The predicate is exactly
    /// [`Mode::LossRate`]'s and fires in the same places; what changes is what
    /// happens when it does. A cut advances the sixty-tick lockout, so a cut
    /// spent on a sub-floor signal is one the window rule cannot spend when
    /// the window actually fills -- which is what the measurement showed: at
    /// two percent loss the predicate produced *fewer* decreases than the
    /// incumbent, a *higher* final rate and *less* delivered throughput.
    /// [`Controller::ease`] lowers the rate by a tenth and touches neither
    /// counter, and this mode owns the cadence.
    LossEase,
    /// The incumbent's own predicate, with the congested-tick counter cleared
    /// on every clean tick instead of every thirtieth.
    ///
    /// **Not a predicate at all: the same signal, answered differently.** The
    /// counter carrying across separated episodes means a second burst
    /// arriving before thirty clean ticks have passed is inside the first
    /// one's period and cannot cut. Clearing it makes every episode
    /// answerable at once, which is what "intermittent congestion is
    /// under-reacted to" means in the improvements plan.
    ///
    /// **Measured 2026-09-07: the carry-over is real and unreachable.** Over
    /// a bursty profile -- fifty milliseconds out in every four hundred --
    /// the run has 17 congested episodes and takes **17 cuts**, and the clean
    /// gaps between episodes are never shorter than thirty frames. The
    /// counter is therefore already cleared by the time each episode arrives,
    /// so clearing it sooner changes nothing, and every profile in this file
    /// is bit-identical between the two.
    ///
    /// Either congestion is continuous, and the sixty-tick period is the
    /// throttle it is meant to be, or it is isolated, and the clean run
    /// between episodes is hundreds of frames long. **The band where the
    /// carry-over bites -- a clean run of one to twenty-nine frames -- did
    /// not occur once.** The behaviour is pinned by
    /// `a_congested_run_is_answered_once_until_its_ticks_are_forgotten` in
    /// the controller's own tests; the concern is theoretical.
    EpisodeReset,
    /// The incumbent, plus a gap in acknowledgements read as congestion.
    ///
    /// **The one sub-floor signal that is unambiguous by construction.** A
    /// peer answers accepted data inside a ten-millisecond floor, so a gap an
    /// order of magnitude past that, with data outstanding, is not reorder and
    /// not jitter. Unlike the gradient this harness can exercise it: the
    /// link's byte budget is one budget for both directions, so a forward
    /// stream that exceeds it starves the return path and the silence is real.
    ///
    /// **Measured 2026-09-07 and not adoptable, for a structural reason.** It
    /// fires: on a half-second total outage the gap reaches 525 ms and the
    /// predicate is true on 131 ticks, 19 of which the window rule calls
    /// clean. How far into each outage each one first answers:
    ///
    /// | ceiling | window rule | silence |
    /// |---|---|---|
    /// | 30 Mibit/s | 40 to 100 ms | 100 to 120 ms |
    /// | 2 Mibit/s | 220 to 240 ms | 100 to 120 ms |
    ///
    /// At the high rate **the window rule is the faster of the two** and the
    /// signal is redundant. At the low rate silence answers 120 ms sooner --
    /// and buys a tenth of a percent of delivered throughput for it, which is
    /// nothing. The reason is
    /// [`Controller::cut`]: the first congested tick of a run cuts and then
    /// every sixtieth does, so an earlier signal moves *when* the single cut
    /// lands and not how many land or how deep they go. Nothing crosses the
    /// path during the outage either way, so 120 ms of earlier cutting is
    /// 120 ms of a rate nobody was using.
    ///
    /// **The general result, which constrains every remaining candidate:** a
    /// sub-floor signal that merely fires *sooner* than the window rule buys
    /// nothing. It has to fire where the window rule fires **never**, and a
    /// total outage is not that case, because the window fills eventually at
    /// any rate this harness can produce.
    AckSilence,
    /// The incumbent, plus a round-trip gradient that **declines to climb**
    /// while the queue is building. It never cuts: the worst it can do is
    /// hold the rate where it is, which is why it is the first shape worth
    /// trying below the window floor.
    ///
    /// **Measured 2026-09-07 and not adoptable as shaped.** It passes the
    /// control the loss-rate predicate failed -- bit-identical to the
    /// incumbent on all three clean paths -- and it is inert where it was
    /// most wanted: at a 4 Mibit/s cap it changes nothing at all. At 8 it
    /// delivers 5.70 against 5.13 for four fewer cuts, but its nominal rate
    /// settles at 9.56 on a path that carries 8, which is worse overshoot
    /// than the incumbent it replaces. Under loss it is neutral at one and
    /// two percent and **loses eleven percent of delivered throughput at
    /// five**. A sixteenfold longer minimum-RTT window was tried and moved
    /// none of it, so the window length is not the reason.
    ///
    /// **Corrected the same day: this harness cannot exercise the signal.**
    /// [`lowlat_sim::Link`]'s capacity is a policer, not a queue -- an
    /// affordable datagram leaves at the link's own delay and an unaffordable
    /// one drops -- so **standing queuing delay is deliberately not modelled**
    /// and there is no queue here for a gradient to see. The rows above are
    /// therefore not a verdict on the signal. Whatever srtt rise it reacted to
    /// at cap=8 comes from retransmissions inflating an unfiltered first-send
    /// sample, which is a different quantity wearing the gradient's clothes.
    /// The clean-path result stands on its own: whatever it reads, it does not
    /// disturb a path that is climbing correctly.
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

/// How long the peer may say nothing, with data outstanding, before the
/// silence is read as congestion.
///
/// **An order of magnitude past the cadence the peer guarantees.** It answers
/// accepted data once ten milliseconds have passed since its last
/// acknowledgement of either kind, so ten times that is not a peer being
/// quiet, and reacting to it does not wait for the retransmission scan.
const ACK_SILENCE_MS: f64 = 100.0;

/// Frames between one gentle reduction and the next.
///
/// **The cadence belongs to the caller**, because easing has no lockout of
/// its own and would otherwise compound on every tick the predicate is true.
/// Half a second at sixty frames, which is the period the increase already
/// runs at.
const EASE_PERIOD: u64 = 30;

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
    /// How long the path is out, and how often it goes out. Zero is never.
    ///
    /// **Nothing crosses in either direction while it is out**, which is the
    /// one profile an acknowledgement-silence signal exists for and the one
    /// the loss and capacity profiles cannot stand in for: a policer drops the
    /// datagrams it cannot afford and lets the small ones through, so
    /// acknowledgements keep arriving however saturated the path is.
    outage_ms: f64,
    outage_every_ms: f64,
    /// Frames a second, which is also the controller's tick rate.
    fps: f64,
    /// The rate to switch to, and when. **The idle desktop waking up**: a
    /// stream encoded at a few frames a second because nothing is changing,
    /// and then something changes. Zero never switches.
    fps_after: f64,
    fps_at_ms: f64,
    /// The controller's ceiling. **The multi-guest shape**: one encode is
    /// divided by the seats sharing it, so each guest's ceiling is a fraction
    /// of the configured rate and its window is a fraction of the fragments.
    /// Low enough, and the window never reaches its floor at all.
    max_mbps: f64,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            loss: 0.0,
            capacity_mibps: 0.0,
            jitter_ms: JITTER_MS,
            reorder: 0.0,
            reorder_ms: 0.0,
            outage_ms: 0.0,
            outage_every_ms: 0.0,
            fps: FRAME_FPS,
            fps_after: 0.0,
            fps_at_ms: 0.0,
            max_mbps: 30.0,
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
    let mut controller = Controller::new(congestion::DEFAULT_LEVEL, 1.0, profile.max_mbps);
    let mut applied_mbps = controller.rate_mbps();
    let mut meter = LossMeter {
        first_sends: 0,
        timeouts: 0,
    };

    let mut wire = [0u8; SLOT + 64];
    let mut scratch = [0u8; SLOT + 64];
    let mut body = [0u8; SLOT];

    let mut frame_ms = 1000.0 / profile.fps;
    let mut next_frame_ms = 0.0;
    let mut measure_at_ms = 0.0;
    let mut offered_bytes_at_mark = 0u64;
    let mut delivered_bytes_at_mark = 0u64;
    let mut measured_offered = 0.0;
    let mut measured_delivered = 0.0;
    let mut below_mean_ms = 0.0;
    let mut eased_at_tick = 0u64;
    let mut tick_index = 0u64;
    let mut eases = 0u64;
    let mut rate_sum = 0.0;
    let mut ticks = 0u64;

    while sim.now_ms() < duration_ms {
        let now = sim.now_ms();

        // The frame offer: as many whole fragments as the applied rate buys
        // in one interval, and a partial one for the rest.
        // **The desktop waking up.** Frames arrive faster, and because the
        // controller's periods are counted in frames rather than in time,
        // its clock speeds up with them.
        if profile.fps_after > 0.0 && now >= profile.fps_at_ms {
            frame_ms = 1000.0 / profile.fps_after;
        }
        let frame = now >= next_frame_ms;
        if frame {
            next_frame_ms = now + frame_ms;
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a positive rate over a frame interval"
            )]
            let frame_bits = (applied_mbps * 1_048_576.0 * (frame_ms / 1000.0)) as usize;
            let mut needed = fragment_count(frame_bits / 8).max(1);
            while needed > 0 {
                if tx.send_message(CHANNEL, &[], &body).is_err() {
                    break;
                }
                needed -= 1;
            }
        }

        // **The path, while it is out, carries nothing either way.** The
        // sender keeps offering and keeps emitting; the datagrams are simply
        // never handed to the link, which is what an outage looks like from
        // both ends.
        let out = profile.outage_every_ms > 0.0
            && (now % profile.outage_every_ms) < profile.outage_ms;
        while let Some(result) = tx.get_output(now, &mut wire) {
            let len = result.expect("sender emitted a malformed datagram");
            if !out {
                sim.send(tx_host, addr(20), 64, &wire[..len]);
            }
        }
        while let Some(result) = rx.get_output(now, &mut wire) {
            let len = result.expect("receiver emitted a malformed datagram");
            if !out {
                sim.send(rx_host, addr(10), 64, &wire[..len]);
            }
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
            let last_ack_ms = tx.last_ack_in_ms();
            if let Some(pressure) = tx.send_pressure(CHANNEL) {
                let loss_ratio = meter.take(&pressure);
                // **With data outstanding**, or an idle channel reads as an
                // outage: a peer with nothing to acknowledge answers on the
                // keepalive cadence, which is slower than this threshold.
                let silent = mode == Mode::AckSilence
                    && pressure.window > 0
                    && now - last_ack_ms > ACK_SILENCE_MS;
                let congested = controller.is_congested(pressure.window, pressure.stale)
                    || (mode == Mode::LossRate && loss_ratio > 0.05)
                    || silent;
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
                // **Clearing on every clean tick, not only the first.** The
                // two are the same thing: what matters is that a clean tick
                // leaves no congested ticks behind for the next episode to
                // inherit, and clearing an already-empty counter is free.
                if mode == Mode::EpisodeReset && !congested {
                    controller.forget_congested_ticks();
                }
                // **The gentle answer, on its own cadence.** The window rule
                // still answers for itself through `tick_as` below; this only
                // adds a reduction where the window rule sees nothing.
                tick_index += 1;
                if mode == Mode::LossEase
                    && loss_ratio > 0.05
                    && !controller.is_congested(pressure.window, pressure.stale)
                    && tick_index.saturating_sub(eased_at_tick) >= EASE_PERIOD
                {
                    controller.ease();
                    eased_at_tick = tick_index;
                    eases += 1;
                }
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
                    below_mean_ms += frame_ms;
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

    if mode == Mode::LossEase {
        eprintln!("    [eases] {eases}");
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
            Mode::AckSilence,
            Mode::LossEase,
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
            Mode::AckSilence,
            Mode::LossEase,
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
            Mode::AckSilence,
            Mode::LossEase,
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

/// **The false cut this profile was built to hold, and the reshape that
/// removed it.**
///
/// Two milliseconds of delay spread, no loss, no cap. That spread exceeds one
/// fragment interval at this rate, so fragments arrive out of order and a
/// receiver names one more than two past its frontier as a negative
/// acknowledgement. **Counting those as loss threw a clean thirty-megabit
/// path down to 4.82 in eleven cuts** while the incumbent held at the
/// ceiling.
///
/// **Reshaped 2026-09-07: the numerator is timeouts alone.** A reorder fires
/// a negative acknowledgement and no timeout, because the gap fills on its
/// own; a loss fires both. The two causes were already separated in the ring
/// and the predicate was summing them. With the reshape this path is
/// bit-identical to the incumbent again, which is what the assertion now
/// says.
#[test]
fn the_loss_rate_candidate_no_longer_false_cuts_on_jitter() {
    let profile = Profile::default();
    let incumbent = run(0x1177, profile, 30_000.0, Mode::Incumbent);
    let candidate = run(0x1177, profile, 30_000.0, Mode::LossRate);

    assert_eq!(
        incumbent.decreases, 0,
        "the control moved: this profile is not clean and proves nothing"
    );
    assert_eq!(
        candidate.decreases, 0,
        "the loss-rate predicate cut a path that lost nothing; the reorder-driven \
         negative acknowledgements are reaching the numerator again"
    );
    assert!(
        (candidate.final_mbps - incumbent.final_mbps).abs() < 1e-9,
        "the predicate moved a clean path: incumbent={:.2} candidate={:.2}",
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

/// **A tick is a frame, so the periods are wall-clock only at sixty of them.**
///
/// Thirty clean ticks between increases is half a second at 60 fps and six
/// seconds at 5. The same clean path is climbed at both, from the same floor
/// to the same ceiling, and what the line carries is how far each one gets in
/// the same thirty seconds of wall clock.
///
/// **This is the idle path's shape.** A desktop that is not changing is
/// encoded at a few frames a second, and a stream that was cut while busy
/// recovers at whatever rate it is now running at -- so the recovery a
/// sixty-frame design intends in half a second takes the better part of a
/// minute.
#[test]
fn trajectories_at_a_low_frame_rate() {
    for fps in [60.0, 5.0] {
        let outcome = run(
            0x3EED,
            Profile {
                fps,
                ..Profile::default()
            },
            30_000.0,
            Mode::Incumbent,
        );
        println!(
            "fps={fps:.0} Incumbent: final={:.2} Mibit/s offered={:.2} delivered={:.2} \
             decreases={} below_mean={:.0} ms",
            outcome.final_mbps,
            outcome.offered_mbps,
            outcome.delivered_mbps,
            outcome.decreases,
            outcome.below_mean_ms
        );
    }
}

/// **The mitigation the frame-counted period has, measured.**
///
/// Fifteen seconds at five frames a second and then fifteen at sixty, against
/// a run that spent all thirty at five and one that spent all thirty at
/// sixty. The middle one is the idle desktop that wakes up.
///
/// **The tick rate scales with the thing the rate is for.** While frames are
/// scarce the controller climbs slowly, and while frames are scarce almost no
/// bitrate is being asked for; the moment something needs the bitrate there
/// are frames again and the periods are back to half a second. Measured
/// 2026-09-07: **3.10 all-slow, 19.60 woken, 30.00 all-fast.** Fifteen
/// seconds of frames recovers most of the distance, so the fps-dependence is
/// real and self-limiting, and it is not worth a divergence.
#[test]
fn trajectories_when_the_frame_rate_recovers() {
    let slow = run(
        0x3EED,
        Profile {
            fps: 5.0,
            ..Profile::default()
        },
        30_000.0,
        Mode::Incumbent,
    );
    let woken = run(
        0x3EED,
        Profile {
            fps: 5.0,
            fps_after: 60.0,
            fps_at_ms: 15_000.0,
            ..Profile::default()
        },
        30_000.0,
        Mode::Incumbent,
    );
    let fast = run(0x3EED, Profile::default(), 30_000.0, Mode::Incumbent);

    for (name, outcome) in [("slow", &slow), ("woken", &woken), ("fast", &fast)] {
        println!(
            "{name}: final={:.2} Mibit/s offered={:.2} delivered={:.2} decreases={}",
            outcome.final_mbps, outcome.offered_mbps, outcome.delivered_mbps, outcome.decreases
        );
        assert_eq!(
            outcome.decreases, 0,
            "{name}: the path lost nothing, so a cut means this measures something else"
        );
    }

    // **The frame count is the clock**, so a run that never gets frames never
    // gets the increases either.
    assert!(
        slow.final_mbps < fast.final_mbps / 5.0,
        "the frame rate stopped mattering: slow={:.2} fast={:.2}",
        slow.final_mbps,
        fast.final_mbps
    );
    // **And the recovery is prompt when they arrive.** Half the run at sixty
    // recovers most of the distance the slow run never travelled.
    assert!(
        woken.final_mbps > slow.final_mbps * 3.0,
        "waking up did not recover the rate: slow={:.2} woken={:.2}",
        slow.final_mbps,
        woken.final_mbps
    );
    // It does not catch up completely, which is the cost that remains.
    assert!(
        woken.final_mbps < fast.final_mbps,
        "waking up cost nothing at all: woken={:.2} fast={:.2}",
        woken.final_mbps,
        fast.final_mbps
    );
}

/// **Intermittent congestion, which the cut arithmetic answers once.**
///
/// A hundred milliseconds out in every four hundred: the episodes are
/// separated, but by eighteen clean frames rather than thirty, so the
/// congested ticks from one are still on the counter when the next arrives
/// and the second episode is inside the first one's period. The incumbent
/// therefore answers a burst pattern roughly as often as it answers one long
/// outage, however many bursts there are.
///
/// `EpisodeReset` clears the counter on every clean tick, so each burst is
/// answerable at once.
///
/// **They print the same numbers, and the reason is the finding.** The clean
/// runs between episodes here are hundreds of frames, far past the thirty
/// that clear the counter anyway, so there is nothing left to carry. See
/// [`Mode::EpisodeReset`].
#[test]
fn trajectories_under_intermittent_congestion() {
    for mode in [Mode::Incumbent, Mode::EpisodeReset] {
        let outcome = run(
            0xB025,
            Profile {
                outage_ms: 100.0,
                outage_every_ms: 400.0,
                ..Profile::default()
            },
            30_000.0,
            mode,
        );
        println!(
            "bursty {mode:?}: final={:.2} Mibit/s offered={:.2} delivered={:.2} \
             decreases={} below_mean={:.0} ms",
            outcome.final_mbps,
            outcome.offered_mbps,
            outcome.delivered_mbps,
            outcome.decreases,
            outcome.below_mean_ms
        );
    }
}

/// **The profile the acknowledgement-silence signal exists for**, and the one
/// no other profile stands in for.
///
/// Half a second of total outage every five seconds. Loss cannot produce this
/// and neither can the capacity cap: a policer drops what it cannot afford
/// and the small return datagrams keep fitting, so acknowledgements arrive
/// throughout however saturated the path is. Only a path that carries nothing
/// makes the peer go quiet.
///
/// What the line carries is **how long each shape takes to answer**. The
/// incumbent cannot react until the send window fills and the scan calls the
/// fragments stale; the silence reads the same outage a tenth of a second in,
/// without waiting for either.
#[test]
fn trajectories_under_an_outage() {
    // **Two rate regimes, because the signal is only interesting in one.**
    // Uncapped, the stream reaches fifteen megabits and the send window
    // passes its floor within a few frames of the path going quiet, so the
    // incumbent has already declared congestion before a tenth of a second of
    // silence has accrued. Capped low, a frame is two fragments and the whole
    // outage does not fill a hundred, so the window rule is blind for the
    // entire gap and this is the only signal that sees it.
    // **Two rate regimes.** The second is the multi-guest shape: a clean path
    // whose ceiling is a share of one encode, small enough that the window
    // never reaches its floor, which is where the incumbent is blind by
    // construction and not merely slow.
    for (name, capacity, ceiling) in [("uncapped ", 0.0, 30.0), ("sub-floor", 0.0, 2.0)] {
        for mode in [
            Mode::Incumbent,
            Mode::LossRate,
            Mode::GoodputPeak,
            Mode::Gradient,
            Mode::AckSilence,
            Mode::LossEase,
        ] {
            let outcome = run(
                0x0FF0,
                Profile {
                    capacity_mibps: capacity,
                    max_mbps: ceiling,
                    outage_ms: 500.0,
                    outage_every_ms: 5_000.0,
                    ..Profile::default()
                },
                30_000.0,
                mode,
            );
            println!(
                "outage {name} {mode:?}: final={:.2} Mibit/s offered={:.2} delivered={:.2} \
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

/// **The finding: the silence is earlier and it does not matter.**
///
/// A clean path whose ceiling is a share of one encode, with the path out for
/// half a second in every five. The window rule is not blind here -- it
/// answers each outage 220 ms in -- and the silence answers at 100. Both
/// produce one cut per outage, and delivered throughput moves by under a
/// tenth of a percent, because the cut arithmetic acts on the first congested
/// tick of a run and then only every sixtieth.
///
/// **This test fails if a reshape makes the silence change the outcome**,
/// which is what it would have to do to earn adoption. Rewrite it into the
/// new expectation rather than deleting it.
#[test]
fn ack_silence_answers_an_outage_sooner_and_the_outcome_is_the_same() {
    // **The uncapped regime, because it is the one that can be shown.** At a
    // two-megabit ceiling the silence answers each outage 120 ms sooner than
    // the window rule and the outcome still does not move -- but after the
    // loss predicate was reshaped, *nothing* moves that profile, so it cannot
    // carry its own denominator. That observation is printed by
    // `trajectories_under_an_outage` rather than asserted here.
    let profile = Profile {
        outage_ms: 500.0,
        outage_every_ms: 5_000.0,
        ..Profile::default()
    };
    let incumbent = run(0x0FF0, profile, 30_000.0, Mode::Incumbent);
    let candidate = run(0x0FF0, profile, 30_000.0, Mode::AckSilence);

    assert!(
        incumbent.decreases > 0,
        "the window rule answered nothing, so this profile shows a blind spot rather \
         than the redundancy it is here to show"
    );
    // **The denominator this finding needs.** "Nothing moved" is worth
    // nothing unless something can move. Lowering the silence threshold as
    // far as the acknowledgement cadence itself still moves nothing, so the
    // absence is not a matter of sensitivity in the signal; a different
    // predicate reaching a different answer on the same traffic is what says
    // the profile can register one at all.
    let sensitive = run(0x0FF0, profile, 30_000.0, Mode::Gradient);
    assert!(
        (sensitive.delivered_mbps - incumbent.delivered_mbps).abs()
            / incumbent.delivered_mbps
            > 0.05,
        "no predicate moves this profile, so it cannot show that one does not"
    );
    assert_eq!(
        candidate.decreases, incumbent.decreases,
        "the silence changed how often the rate was cut; the finding has moved"
    );
    // **Half a percent, not equality.** Cutting 120 ms earlier does shift the
    // recovery's phase, so the two runs are not bit-identical; what the
    // finding says is that the shift is not worth having, and a candidate
    // that earned adoption would move this by far more than the margin.
    let moved = (candidate.delivered_mbps - incumbent.delivered_mbps).abs()
        / incumbent.delivered_mbps;
    assert!(
        moved < 0.005,
        "the silence moved delivered throughput by {:.2}%: incumbent={:.4} candidate={:.4}",
        moved * 100.0,
        incumbent.delivered_mbps,
        candidate.delivered_mbps
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
