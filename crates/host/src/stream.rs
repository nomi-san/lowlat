//! The capture and encode loop, and the seats guests take on it.
//!
//! **One capture and one encode serve every guest** (docs/05-host.md section
//! 1). Per-guest work begins at packetization, where the sequence spaces and
//! the rings are already separate, so everything above that line lives here on
//! one thread and everything below it lives on the guest's own.
//!
//! The loop owns the source, the encoder, the frame pool, the delivery gate
//! and the bitrate budget. A guest owns its seat: a bounded ring of published
//! pool indices coming in, and three counters of transport pressure going out.
//!
//! # The seat protocol
//!
//! A seat moves through four states and **each transition has exactly one
//! owner**, which is what keeps the handoff free of locks:
//!
//! ```text
//!   Free  --guest claims-->  Claimed  --loop admits-->  Streaming
//!     ^                                                     |
//!     +---------- loop drains ---- Leaving <---- guest leaves
//! ```
//!
//! The loop promotes `Claimed` to `Streaming` at the top of a frame, before
//! the gate runs, so the set of seats a frame is delivered to is fixed for
//! that frame and a guest that arrives mid-frame waits for the next one. That
//! is what stops a joining guest being handed a predicted frame before the
//! gate has latched it to a keyframe.
//!
//! Teardown runs the other way. A leaving guest stops touching its ring and
//! marks the seat, and **the loop is what empties it**, because the loop is
//! the only thing that pushes: once it sees `Leaving` it publishes nothing
//! more there, releases whatever is still queued, and only then frees the
//! seat. A guest draining its own ring would race the push that is already in
//! flight, and every index lost that way is a pool slot that never comes back.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;

use lowlat_capture::synthetic::Synthetic;
use lowlat_common::spsc::Ring;
use lowlat_encode::{Encoder, Poll};
use lowlat_net::WakeHandle;

use crate::frames::{self, Pool};
use crate::gate::{self, Gate, Keyframe};
use lowlat_core::congestion::Controller;

use crate::rate::{Budget, Sample};

/// Seats, and therefore the compile-time guest cap.
pub const MAX_SEATS: usize = gate::MAX_GUESTS;

/// How many encoded frames may be queued to one guest before the gate has to
/// account for it.
///
/// Small on purpose. A guest that is more than a few frames behind is not
/// going to catch up by being given more buffer; it is going to be latched by
/// the gate and resynchronised on a keyframe, and a deep queue only delays
/// that decision while spending pool slots.
const PUBLISH_DEPTH: usize = 2;

/// Encoded frames held at once.
///
/// One is being written, the rest are in flight to guests.
///
/// **Deliberately fewer than the guests could hold between them.** Sizing the
/// pool so exhaustion is impossible would mean a slot per guest per queued
/// frame, at the width of the largest frame a window can carry, which is tens
/// of megabytes for guests that are keeping up and need none of it. Exhaustion
/// is a legitimate back-pressure signal instead: it latches every guest and
/// asks for the refresh that recovers them, which is the same answer a full
/// window gets.
const POOL_SLOTS: usize = 4;

/// How long the loop waits between asking the encoder again.
///
/// **One millisecond, never less.** A shorter sleep is a busy wait with extra
/// steps, and what it is waiting for is an encode measured in milliseconds. A
/// not-ready answer costs a driver round trip, so the polls themselves are not
/// what needs pacing; the sleep is there so the thread is not spinning while
/// the hardware works.
const COLLECT_WAIT: std::time::Duration = std::time::Duration::from_millis(1);

/// What a seat is doing. See the module note; each transition has one owner.
mod seat_state {
    pub(super) const FREE: u32 = 0;
    pub(super) const CLAIMED: u32 = 1;
    pub(super) const STREAMING: u32 = 2;
    pub(super) const LEAVING: u32 = 3;
}

/// One guest's place on the stream.
#[derive(Debug)]
struct Seat {
    /// Pool indices, loop to guest. **Single producer, single consumer**: the
    /// loop is the only pusher and the seat's guest the only popper.
    ring: Ring<u32, PUBLISH_DEPTH>,
    state: AtomicU32,
    /// Fragments between the send base and the send next, on the video
    /// channel. The gate's room test and the controller's window are the same
    /// number, read once per frame.
    window: AtomicU32,
    /// How many of them the sender's scan called stale.
    stale: AtomicU32,
    /// Throughput since the last increase, as `f32` bits. A float in an atomic
    /// rather than a scaled integer, because the controller takes a float and
    /// a fixed-point round trip would quietly change what it is given.
    measured_bits: AtomicU32,
    /// Frames this guest lost after the gate had already admitted them.
    ///
    /// **The gate lives on the loop's thread, so a guest cannot latch itself.**
    /// A send the transport refuses is a broken reference chain exactly as a
    /// full window is, and the guest has no other way to say so.
    missed: AtomicU32,
}

impl Seat {
    fn new() -> Self {
        Self {
            ring: Ring::new(),
            state: AtomicU32::new(seat_state::FREE),
            window: AtomicU32::new(0),
            stale: AtomicU32::new(0),
            measured_bits: AtomicU32::new(0),
            missed: AtomicU32::new(0),
        }
    }
}

/// What a guest sends the loop when it takes a seat.
#[derive(Debug)]
struct Join {
    seat: usize,
    /// How to wake that guest's loop once a frame is published to it. The
    /// handle owns its own descriptor, so it stays valid after the guest's own
    /// wake is gone and a late notify writes to a descriptor nobody reads
    /// rather than to whatever inherited the number.
    wake: WakeHandle,
}

/// Everything both sides of the handoff can see.
#[derive(Debug)]
struct Shared {
    seats: [Seat; MAX_SEATS],
    pool: Pool,
    /// Raised to end the loop; the loop checks it once per frame.
    stopping: AtomicU32,
}

/// How the stream is configured. Fixed for its lifetime.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// What the operator asked for, before it is divided among guests.
    pub configured_mbps: f64,
    /// The floor a controller may not descend below.
    pub min_mbps: f64,
}

/// The running loop, and the handle the seam holds it by.
#[derive(Debug)]
pub struct Stream {
    shared: Arc<Shared>,
    joins: Option<mpsc::Sender<Join>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stream {
    /// Start the loop.
    ///
    /// It parks while no seat is taken, so a host with no guests holds no
    /// encoder and no hardware. The encoder is built on the first guest and
    /// released with the last, on this thread, because that is where the
    /// device context has to live.
    pub fn start(config: Config) -> Self {
        let shared = Arc::new(Shared {
            seats: core::array::from_fn(|_| Seat::new()),
            pool: Pool::new(POOL_SLOTS, max_frame_bytes()),
            stopping: AtomicU32::new(0),
        });
        let (joins, arrivals) = mpsc::channel();
        let owned = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("lowlat-stream".to_string())
            .spawn(move || run(&owned, &arrivals, config))
            .ok();
        Self {
            shared,
            joins: Some(joins),
            thread,
        }
    }

    /// A handle for one guest's thread.
    ///
    /// Cloning the shared state costs one reference count per session, on the
    /// approval path. Nothing on a frame path touches it again.
    pub fn seats(&self) -> Seats {
        Seats {
            shared: Arc::clone(&self.shared),
            joins: self.joins.clone(),
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.shared.stopping.store(1, Ordering::Release);
        // Dropped before the join so the loop's receive ends rather than
        // waiting on a sender that is still alive.
        self.joins = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A guest thread's way onto the stream.
#[derive(Debug, Clone)]
pub struct Seats {
    shared: Arc<Shared>,
    joins: Option<mpsc::Sender<Join>>,
}

impl Seats {
    /// Take a seat, or `None` when every one is occupied.
    ///
    /// Called by the guest's thread once it is streamable, never before: a
    /// seat taken by a guest that has not declared itself is a share of the
    /// bitrate budget spent on a peer that may never decode anything.
    pub fn take(&self, wake: WakeHandle) -> Option<SeatHold> {
        let joins = self.joins.as_ref()?;
        for (index, seat) in self.shared.seats.iter().enumerate() {
            if seat
                .state
                .compare_exchange(
                    seat_state::FREE,
                    seat_state::CLAIMED,
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                seat.window.store(0, Ordering::Relaxed);
                seat.stale.store(0, Ordering::Relaxed);
                seat.measured_bits.store(0, Ordering::Relaxed);
                seat.missed.store(0, Ordering::Relaxed);
                if joins.send(Join { seat: index, wake }).is_err() {
                    seat.state.store(seat_state::FREE, Ordering::Release);
                    return None;
                }
                return Some(SeatHold {
                    shared: Arc::clone(&self.shared),
                    index,
                });
            }
        }
        None
    }
}

/// One guest's hold on a seat, for as long as it streams.
///
/// **Releases itself**, in the same shape as a hold on a pool slot: the seat
/// comes back on drop, whatever path the guest's thread left by.
#[derive(Debug)]
pub struct SeatHold {
    shared: Arc<Shared>,
    index: usize,
}

impl SeatHold {
    /// The next published frame, or `None`.
    ///
    /// The frame releases its hold on the pool slot when it is dropped, so a
    /// path that returns early cannot leak one.
    pub fn next_frame(&self) -> Option<frames::Frame<'_>> {
        let seat = self.shared.seats.get(self.index)?;
        let index = seat.ring.pop()?;
        self.shared.pool.claim(index)
    }

    /// Report this guest's transport pressure. Once per pass, from the loop
    /// that already has the numbers.
    pub fn report(&self, window: u32, stale: u32, measured_mbps: f64) {
        let Some(seat) = self.shared.seats.get(self.index) else {
            return;
        };
        seat.window.store(window, Ordering::Relaxed);
        seat.stale.store(stale, Ordering::Relaxed);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a throughput figure; f32 range covers it and the controller takes a float"
        )]
        seat.measured_bits
            .store((measured_mbps as f32).to_bits(), Ordering::Relaxed);
    }

    /// A frame the gate admitted did not reach the wire.
    ///
    /// The loop latches this guest on its next pass. Reported rather than
    /// handled here, because the gate is the only thing allowed to latch and
    /// it lives on the other thread.
    pub fn missed_frame(&self) {
        if let Some(seat) = self.shared.seats.get(self.index) {
            seat.missed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for SeatHold {
    /// Hand the seat back. **The loop empties the ring, not this**; see the
    /// module note on why the leaving side must not.
    fn drop(&mut self) {
        if let Some(seat) = self.shared.seats.get(self.index) {
            seat.state.store(seat_state::LEAVING, Ordering::Release);
        }
    }
}

/// The largest frame that can reach a guest at all.
///
/// Not an estimate: a frame past this cannot be enqueued, because the send
/// window is the peer's ring depth and the length prefix and video header ride
/// inside it. Sizing a pool slot above it would buy storage for a frame that
/// could only ever be refused.
fn max_frame_bytes() -> usize {
    let body = lowlat_core::DEFAULT_DATAGRAM
        - lowlat_core::envelope::ENVELOPE_LEN
        - lowlat_core::packet::HEADER_LEN;
    gate::ceiling(f32::MAX) as usize * body
        - lowlat_core::message::LENGTH_PREFIX_LEN
        - lowlat_core::video::VIDEO_HEADER_LEN
}

/// One guest, as the loop sees it.
struct Active {
    seat: usize,
    wake: WakeHandle,
}

/// The loop.
fn run(shared: &Shared, arrivals: &mpsc::Receiver<Join>, config: Config) {
    // Waiting rather than holding hardware. A host advertises itself long
    // before anyone connects, and an encoder open across that whole time is a
    // device another application cannot have.
    while !occupied(shared) {
        if shared.stopping.load(Ordering::Acquire) != 0 {
            return;
        }
        std::thread::sleep(IDLE_WAIT);
    }

    let Ok(display) = lowlat_encode::vaapi::Vaapi::load() else {
        lowlat_common::log_error!("stream: display runtime unavailable, nothing will encode");
        return;
    };
    let Ok(display) = display.open(c"/dev/dri/renderD128") else {
        lowlat_common::log_error!("stream: render node could not be opened");
        return;
    };
    let Ok(caps) = display.caps(lowlat_encode::vaapi::Codec::H264) else {
        lowlat_common::log_error!("stream: render node reports no h264 encode");
        return;
    };
    let Ok(context) = display.create_context(caps, config.width, config.height, ENCODE_DEPTH)
    else {
        lowlat_common::log_error!("stream: encode context could not be created");
        return;
    };
    let params = lowlat_encode::h264::Params {
        width: config.width,
        height: config.height,
        fps: config.fps,
        level_idc: 42,
        log2_max_frame_num_minus4: 4,
        log2_max_poc_lsb_minus4: 4,
        max_num_ref_frames: 1,
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a configured bitrate in megabits, well inside u32 as bits per second"
    )]
    let start_bps = (config.configured_mbps * 1_000_000.0) as u32;
    let Ok(mut encoder) = context.encoder(params, start_bps) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return;
    };

    lowlat_common::log_info!(
        "stream: encoding w={} h={} fps={} ceiling_mbps={:.1}",
        config.width,
        config.height,
        config.fps,
        config.configured_mbps
    );
    encode_loop(shared, arrivals, config, &mut encoder);
}

/// How long the loop sleeps while no guest is seated.
const IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Pictures the encoder may hold at once.
const ENCODE_DEPTH: usize = 4;

fn occupied(shared: &Shared) -> bool {
    shared
        .seats
        .iter()
        .any(|seat| seat.state.load(Ordering::Acquire) != seat_state::FREE)
}

/// The frame loop, written against the trait rather than a backend, so the
/// second implementation is a construction change and not a second loop.
fn encode_loop<E: Encoder>(
    shared: &Shared,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    encoder: &mut E,
) {
    let mut source = Synthetic::new(config.width, config.height);
    let mut gate = Gate::new();
    let mut budget = Budget::new(config.configured_mbps, config.min_mbps);

    // Compact and parallel: one entry per streaming guest, in no particular
    // order. Compaction happens on join and leave, which are rare, so the
    // per-frame work is a walk over exactly the guests that exist.
    let mut active: Vec<Active> = Vec::with_capacity(MAX_SEATS);
    let mut guests: Vec<gate::Guest> = Vec::with_capacity(MAX_SEATS);
    let mut controllers: Vec<Controller> = Vec::with_capacity(MAX_SEATS);
    let mut samples: Vec<Sample> = Vec::with_capacity(MAX_SEATS);

    let started = lowlat_common::clock::Time::now();
    let interval_ms = 1000.0 / f64::from(config.fps.max(1));
    let mut deadline_ms = 0.0f64;
    let mut force_keyframe = false;

    loop {
        if shared.stopping.load(Ordering::Acquire) != 0 {
            return;
        }

        // **Before the gate, and only here.** The set of guests a frame is
        // delivered to is fixed for that frame, so a guest that arrives while
        // one is in flight waits for the next rather than receiving a
        // predicted frame it cannot decode.
        let moved = admit_and_retire(
            shared,
            arrivals,
            config,
            &mut active,
            &mut guests,
            &mut controllers,
        );
        if moved {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "bounded by MAX_SEATS, which is sixteen"
            )]
            budget.rebound(active.len() as u32, &mut controllers);
            // **The gate's ceiling is the divided rate, not the configured
            // one.** A guest's room test scales with what that guest is
            // allowed to send, so a second guest halves both the rate and the
            // window it is measured against.
            #[allow(
                clippy::cast_possible_truncation,
                reason = "a rate in megabits, only used to pick one of three ceiling steps"
            )]
            let ceiling = budget.ceiling() as f32;
            for guest in &mut guests {
                guest.set_rate(ceiling);
            }
        }
        if active.is_empty() {
            std::thread::sleep(IDLE_WAIT);
            deadline_ms = lowlat_common::clock::elapsed_ms(started);
            continue;
        }

        // Pace. The deadline advances by the interval rather than from now, so
        // the frame clock does not drift, and it is pulled forward after a
        // stall rather than trying to catch up on a burst nobody can use.
        let now_ms = lowlat_common::clock::elapsed_ms(started);
        if deadline_ms > now_ms {
            lowlat_common::clock::precise_sleep(std::time::Duration::from_secs_f64(
                (deadline_ms - now_ms) / 1000.0,
            ));
        }
        let now_ms = lowlat_common::clock::elapsed_ms(started);
        deadline_ms = if deadline_ms + interval_ms < now_ms {
            now_ms + interval_ms
        } else {
            deadline_ms + interval_ms
        };

        let frame = source.acquire();
        if encoder.submit(&frame, force_keyframe).is_err() {
            // Queue full is back pressure rather than a fault: collect below
            // and the next frame goes in.
            continue;
        }
        force_keyframe = false;

        // **Between the submit and the collect**, which is the whole reason
        // the trait separates them: this is real work overlapping the encode
        // rather than a loop waiting on it.
        samples.clear();
        for entry in &active {
            let Some(seat) = shared.seats.get(entry.seat) else {
                continue;
            };
            let window = seat.window.load(Ordering::Relaxed);
            samples.push(Sample {
                window,
                stale: seat.stale.load(Ordering::Relaxed),
                measured_mbps: f64::from(f32::from_bits(
                    seat.measured_bits.load(Ordering::Relaxed),
                )),
            });
        }
        for ((guest, sample), entry) in guests.iter_mut().zip(samples.iter()).zip(active.iter()) {
            guest.set_outstanding(sample.window);
            // A frame the transport refused after the gate admitted it. The
            // count is taken rather than read, so one loss latches once.
            if let Some(seat) = shared.seats.get(entry.seat)
                && seat.missed.swap(0, Ordering::Relaxed) != 0
            {
                guest.mark_skipping();
            }
        }
        if let Some(rate_mbps) = budget.tick(&mut controllers, &samples) {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a rate the controller has already clamped to its bounds"
            )]
            let bps = (rate_mbps * 1_000_000.0) as u32;
            // A live change. It reinitialises nothing and forces no refresh,
            // which is what keeps the stream unbroken across a reconfigure.
            let _ = encoder.reconfigure(bps);
        }

        force_keyframe |= collect(
            shared,
            encoder,
            &mut gate,
            &active,
            &mut guests,
            deadline_ms,
            started,
        );
    }
}

/// Promote the guests that have arrived and retire the ones that have gone.
///
/// Returns whether the guest count moved, because that is the event a
/// controller has to be told about rather than discovering on a tick.
fn admit_and_retire(
    shared: &Shared,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    active: &mut Vec<Active>,
    guests: &mut Vec<gate::Guest>,
    controllers: &mut Vec<Controller>,
) -> bool {
    let mut moved = false;

    while let Ok(join) = arrivals.try_recv() {
        let Some(seat) = shared.seats.get(join.seat) else {
            continue;
        };
        // A guest that gave up between claiming and being admitted.
        if seat.state.load(Ordering::Acquire) != seat_state::CLAIMED {
            release(seat, &shared.pool);
            continue;
        }
        seat.state.store(seat_state::STREAMING, Ordering::Release);
        active.push(Active {
            seat: join.seat,
            wake: join.wake,
        });
        // **A fresh guest starts pending**, which is what produces its join
        // keyframe rather than a separate arrangement.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a configured rate in megabits, only used to pick a ceiling step"
        )]
        guests.push(gate::Guest::joining(config.configured_mbps as f32));
        controllers.push(Controller::new(0, config.min_mbps, config.configured_mbps));
        moved = true;
    }

    let mut at = 0;
    while at < active.len() {
        let Some(entry) = active.get(at) else { break };
        let Some(seat) = shared.seats.get(entry.seat) else {
            break;
        };
        if seat.state.load(Ordering::Acquire) != seat_state::LEAVING {
            at += 1;
            continue;
        }
        // Nothing is pushed here any more, so what is left is ours to release.
        // Every index dropped instead is a pool slot that never comes back.
        release(seat, &shared.pool);
        seat.state.store(seat_state::FREE, Ordering::Release);
        active.swap_remove(at);
        guests.swap_remove(at);
        controllers.swap_remove(at);
        moved = true;
    }

    moved
}

/// Empty a seat's ring, giving every queued frame back to the pool.
fn release(seat: &Seat, pool: &Pool) {
    while let Some(index) = seat.ring.pop() {
        drop(pool.claim(index));
    }
}

/// Take whatever the encoder has finished, and deliver each picture.
///
/// Returns whether a keyframe was asked for.
///
/// **Drains rather than taking one.** Submitting one picture and collecting
/// one holds whatever backlog it starts with forever, so a picture that misses
/// its deadline once would cost a frame of latency for the rest of the
/// session. Draining gives that frame back on the next pass that has room.
fn collect<E: Encoder>(
    shared: &Shared,
    encoder: &mut E,
    gate: &mut Gate,
    active: &[Active],
    guests: &mut [gate::Guest],
    deadline_ms: f64,
    started: lowlat_common::clock::Time,
) -> bool {
    let mut wanted = false;
    let mut delivered = 0usize;
    loop {
        match encoder.poll() {
            Err(_) => {
                lowlat_common::log_error!("stream: collect failed");
                return wanted;
            }
            Ok(Poll::Pending) => {
                // Nothing more this pass. The wait is bounded by the frame
                // deadline, so a slow encode costs its own frame and not the
                // ones behind it.
                if delivered > 0 || lowlat_common::clock::elapsed_ms(started) >= deadline_ms {
                    return wanted;
                }
                std::thread::sleep(COLLECT_WAIT);
            }
            Ok(Poll::Ready {
                bitstream,
                keyframe,
            }) => {
                let len = bitstream.len();
                let Some(mut writer) = shared.pool.acquire() else {
                    // Every slot is still held, so every guest is behind. The
                    // frame is dropped for all of them, and they are latched:
                    // a dropped frame breaks the reference chain whatever the
                    // reason for dropping it.
                    //
                    // **And the refresh is asked for here**, because the pass
                    // that would otherwise ask is the one that just failed to
                    // take a slot.
                    let now_ms = lowlat_common::clock::elapsed_ms(started);
                    for guest in guests.iter_mut() {
                        guest.mark_skipping();
                    }
                    wanted |= gate.request_keyframe(now_ms) == Keyframe::Request;
                    return wanted;
                };
                if !writer.fill(bitstream) {
                    // A sizing error rather than a transient one: the slot is
                    // as wide as a deliverable frame can be, so a frame past
                    // it is one no window could have carried either.
                    lowlat_common::log_error!(
                        "stream: coded frame exceeds the pool slot, bytes={len}"
                    );
                    let now_ms = lowlat_common::clock::elapsed_ms(started);
                    for guest in guests.iter_mut() {
                        guest.mark_skipping();
                    }
                    wanted |= gate.request_keyframe(now_ms) == Keyframe::Request;
                    return wanted;
                }

                let now_ms = lowlat_common::clock::elapsed_ms(started);
                let mut picked = [0usize; MAX_SEATS];
                let mut count = 0usize;
                let request = gate.admit(fragments_for(len), keyframe, now_ms, guests, |index| {
                    if let Some(slot) = picked.get_mut(count) {
                        *slot = index;
                        count += 1;
                    }
                });
                wanted |= request == Keyframe::Request;
                deliver(shared, active, &picked, count, guests, writer, keyframe);
                delivered += 1;
            }
        }
    }
}

/// Publish one frame to the guests the gate admitted, and wake them.
fn deliver(
    shared: &Shared,
    active: &[Active],
    picked: &[usize; MAX_SEATS],
    count: usize,
    guests: &mut [gate::Guest],
    writer: frames::Writer<'_>,
    keyframe: bool,
) {
    // Something real to fill the array with, so the publish takes a slice of
    // references without an uninitialised element in it. Only the first `n`
    // are ever read.
    let Some(filler) = shared.seats.first() else {
        return;
    };
    let mut rings: [&Ring<u32, PUBLISH_DEPTH>; MAX_SEATS] = [&filler.ring; MAX_SEATS];
    let mut n = 0usize;
    for at in picked.get(..count).unwrap_or(&[]) {
        let Some(entry) = active.get(*at) else {
            continue;
        };
        let Some(seat) = shared.seats.get(entry.seat) else {
            continue;
        };
        if let Some(target) = rings.get_mut(n) {
            *target = &seat.ring;
            n += 1;
        }
    }

    let taken = writer.publish(keyframe, rings.get(..n).unwrap_or(&[]));

    // **A ring that refused is a guest that missed a frame**, and a guest that
    // misses one frame must miss every frame until a keyframe. The pool gives
    // the hold back on its own, but only the gate can latch, so the shortfall
    // is acted on here rather than counted and forgotten.
    for at in 0..n {
        if taken & (1u32 << at) != 0 {
            continue;
        }
        if let Some(index) = picked.get(at)
            && let Some(guest) = guests.get_mut(*index)
        {
            guest.mark_skipping();
        }
    }

    for at in picked.get(..count).unwrap_or(&[]) {
        if let Some(entry) = active.get(*at) {
            let _ = entry.wake.notify();
        }
    }
}

/// Fragments one coded frame occupies, header and prefix included.
///
/// Exact rather than the `bytes / mtu` a peer uses, which rounds down and
/// accounts for no header. Being one fragment more conservative than the peer
/// is the safe direction for a room test.
fn fragments_for(len: usize) -> u32 {
    let body = lowlat_core::DEFAULT_DATAGRAM
        - lowlat_core::envelope::ENVELOPE_LEN
        - lowlat_core::packet::HEADER_LEN;
    let total =
        len + lowlat_core::video::VIDEO_HEADER_LEN + lowlat_core::message::LENGTH_PREFIX_LEN;
    u32::try_from(total.div_ceil(body)).unwrap_or(u32::MAX)
}

// The loop's own state is plain atomics, but the pool it publishes into is
// the model-checked one, and under the model those primitives may only be
// touched inside a model run. The pool's own loom test is where that
// obligation is met; these drive real threads and real time.
#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// An encoder that answers at once with a canned access unit.
    ///
    /// **The trait is what makes this possible**, and it is the second thing
    /// the trait buys after the two hardware backends: the loop under test is
    /// the same code that drives them, with the hardware's latency and its
    /// device removed.
    #[derive(Debug)]
    struct Fake {
        unit: Vec<u8>,
        /// One entry per picture in flight, true where a refresh was asked
        /// for. **A fake that never answers a refresh request would hold every
        /// guest latched forever**, which looks exactly like the loop not
        /// running, so the answer is part of what makes the harness honest.
        queued: std::collections::VecDeque<bool>,
        /// Refresh requests seen, which is how the recovery path is observed
        /// from outside the gate.
        forced: Arc<AtomicU32>,
    }

    #[derive(Debug)]
    struct Never;

    impl core::fmt::Display for Never {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("never")
        }
    }

    impl core::error::Error for Never {}

    impl Encoder for Fake {
        type Error = Never;

        fn submit(
            &mut self,
            _frame: &lowlat_capture::Frame<'_>,
            force_keyframe: bool,
        ) -> Result<(), Never> {
            if force_keyframe {
                self.forced.fetch_add(1, Ordering::Relaxed);
            }
            self.queued.push_back(force_keyframe);
            Ok(())
        }

        fn poll(&mut self) -> Result<Poll<'_>, Never> {
            let Some(keyframe) = self.queued.pop_front() else {
                return Ok(Poll::Pending);
            };
            Ok(Poll::Ready {
                bitstream: &self.unit,
                keyframe,
            })
        }

        fn reconfigure(&mut self, _bitrate_bps: u32) -> Result<(), Never> {
            Ok(())
        }
    }

    /// A stream with the loop running against the fake, small and fast.
    struct Harness {
        stream: Stream,
        forced: Arc<AtomicU32>,
    }

    impl Harness {
        fn start() -> Self {
            Self::with_pool(POOL_SLOTS)
        }

        /// A pool of a chosen size, which is how the two ways of losing a
        /// frame are told apart. At the real size a guest that consumes
        /// nothing fills its ring first; at one slot the pool runs out while
        /// that ring still has room, so each path can be reached on its own.
        fn with_pool(slots: usize) -> Self {
            let config = Config {
                width: 320,
                height: 240,
                fps: 240,
                configured_mbps: 10.0,
                min_mbps: 1.0,
            };
            let shared = Arc::new(Shared {
                seats: core::array::from_fn(|_| Seat::new()),
                pool: Pool::new(slots, max_frame_bytes()),
                stopping: AtomicU32::new(0),
            });
            let (joins, arrivals) = mpsc::channel();
            let forced = Arc::new(AtomicU32::new(0));
            let owned = Arc::clone(&shared);
            let counter = Arc::clone(&forced);
            let thread = std::thread::Builder::new()
                .name("lowlat-stream-test".to_string())
                .spawn(move || {
                    let mut encoder = Fake {
                        unit: vec![0x41; 900],
                        queued: std::collections::VecDeque::new(),
                        forced: counter,
                    };
                    encode_loop(&owned, &arrivals, config, &mut encoder);
                })
                .expect("thread");
            Self {
                stream: Stream {
                    shared,
                    joins: Some(joins),
                    thread: Some(thread),
                },
                forced,
            }
        }

        fn seat(&self) -> SeatHold {
            let wake = lowlat_net::Wake::new().expect("wake");
            self.stream
                .seats()
                .take(wake.handle().expect("handle"))
                .expect("a free seat")
        }
    }

    /// Spin until `predicate` holds or the budget runs out, so a test reports
    /// what it was waiting for rather than hanging.
    fn until(what: &str, mut predicate: impl FnMut() -> bool) {
        let began = lowlat_common::clock::Time::now();
        while !predicate() {
            assert!(
                lowlat_common::clock::elapsed_ms(began) < 5000.0,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The whole handoff, end to end: a guest takes a seat, the loop admits it
    /// at the top of a frame, and frames arrive.
    #[test]
    fn a_seated_guest_receives_frames() {
        let harness = Harness::start();
        let seat = harness.seat();

        let mut received = 0usize;
        until("frames to arrive", || {
            while let Some(frame) = seat.next_frame() {
                assert_eq!(frame.bytes().len(), 900, "a frame arrived truncated");
                received += 1;
            }
            received >= 8
        });
    }

    /// **The loop empties a leaving seat's ring, and nothing else can.** The
    /// guest stops touching the ring before it is marked, so a push already in
    /// flight lands after that; every index left there is a pool slot that
    /// never comes back, and a host that leaks one per session runs out.
    /// *Named regression test.*
    #[test]
    fn a_guest_that_leaves_strands_no_pool_slot() {
        let harness = Harness::start();
        let seat = harness.seat();

        // Deliberately not consumed, so the ring is holding slots when the
        // seat goes. Consuming them would release the holds by the ordinary
        // path and prove nothing about the drain.
        until("the ring to hold frames", || {
            harness.stream.shared.pool.free_slots() < POOL_SLOTS
        });
        drop(seat);

        until("every slot to come back", || {
            harness.stream.shared.pool.free_slots() == POOL_SLOTS
        });
    }

    /// A frame a guest did not get is a broken reference chain, whatever the
    /// reason it was dropped: a ring that refused, or a pool with nothing
    /// free. Both must latch the guest and both must reach the refresh that
    /// recovers it. Neither the pool path nor the ring path asks for a refresh
    /// on its own, so removing either one fails this. *Named regression test.*
    #[test]
    fn a_guest_whose_ring_fills_is_latched_and_a_refresh_is_asked_for() {
        // The real pool, where a single guest that reads nothing fills its own
        // ring while slots are still free. So the loss here is the publish
        // refusing, and nothing else.
        let harness = Harness::start();
        let _seat = harness.seat();

        // **More than one, and that is the whole test.** A joining guest
        // starts latched and asks for its first refresh on arrival, so a
        // single request says nothing about what happens when a ring fills.
        // The second one can only come from the shortfall being acted on.
        until("a refresh after the join refresh", || {
            harness.forced.load(Ordering::Relaxed) >= 2
        });
    }

    /// The other way to lose a frame: nothing free to copy it into. It latches
    /// every guest, because the frame reached none of them, and it has to ask
    /// for the refresh itself -- the pass that would otherwise ask is the one
    /// that could not take a slot. *Named regression test.*
    #[test]
    fn a_frame_with_no_pool_slot_free_latches_and_asks_for_a_refresh() {
        // One slot, so the first published frame holds it and the next finds
        // nothing free while the guest's ring still has room. That is the pool
        // path with the ring path kept out of it.
        let harness = Harness::with_pool(1);
        let _seat = harness.seat();

        until("a refresh after the join refresh", || {
            harness.forced.load(Ordering::Relaxed) >= 2
        });
    }

    /// The pool slot and the send ring are sized from the same arithmetic, so
    /// a frame that fits a slot is a frame the window can carry. Sizing either
    /// alone lets the loop copy a frame nothing could send.
    #[test]
    fn the_largest_frame_a_slot_holds_is_the_largest_a_window_carries() {
        assert_eq!(
            fragments_for(max_frame_bytes()),
            gate::ceiling(f32::MAX),
            "the pool slot and the peer's ring depth have parted company"
        );
        assert_eq!(
            fragments_for(max_frame_bytes() + 1),
            gate::ceiling(f32::MAX) + 1,
            "the slot is not the largest frame that fits"
        );
    }
}
