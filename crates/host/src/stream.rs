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
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::mpsc;

use lowlat_capture::synthetic::{Marker, Synthetic};
use lowlat_common::spsc::Ring;
use lowlat_encode::{Encoder, Poll};
use lowlat_net::WakeHandle;

use crate::frames::{self, Pool};
use crate::gate::{self, Gate, Keyframe};
use crate::timing::{Report, Stages};
use lowlat_core::congestion::Controller;
use lowlat_core::control::status;

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

/// The same figure as a number, for comparing against a remaining interval.
const POLL_MS: f64 = 1.0;

/// Frames between one timing report and the next. Ten seconds at sixty.
const REPORT_FRAMES: u32 = 600;

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
    /// A refresh this guest asked for outright.
    ///
    /// **A peer that cannot decode says so, and it is the only party that
    /// can.** Its decoder has failed on something the wire delivered intact,
    /// and it recovers by asking for a picture with no history behind it.
    /// Ignoring the request leaves it failing on every frame until it gives
    /// up, which from this side looks like a peer that simply stopped.
    refresh: AtomicU32,
    /// Frames this guest lost after the gate had already admitted them.
    ///
    /// **The gate lives on the loop's thread, so a guest cannot latch itself.**
    /// A send the transport refuses is a broken reference chain exactly as a
    /// full window is, and the guest has no other way to say so.
    missed: AtomicU32,
    /// What this guest declared it can decode, as the video flag bits.
    ///
    /// Written by the guest's thread whenever the declaration changes, read by
    /// the loop when a reconfiguration is asked for.
    flags: AtomicU32,
    /// This guest asked for the encoder to be reinitialized.
    ///
    /// **Not the same as a refresh.** A refresh is a picture with no history
    /// in the stream that is already running; this asks for a different stream
    /// and is answered by building one.
    reconfigure: AtomicU32,
    /// Whether this guest's request is the one currently being tried.
    ///
    /// Moved here out of [`Seat::reconfigure`] when the request is taken, so
    /// that a build which then fails can be reported to the guests that asked
    /// for it and to no others.
    asked_last: AtomicU32,
    /// A reason this guest is being ended, or zero.
    ///
    /// **Zero is not a reason**, which is what makes it usable as the absent
    /// value: a peer stops on a non-zero status and carries on through a zero
    /// one, so there is no status a host would send that could be mistaken for
    /// no status at all.
    kick: AtomicI32,
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
            refresh: AtomicU32::new(0),
            flags: AtomicU32::new(0),
            reconfigure: AtomicU32::new(0),
            asked_last: AtomicU32::new(0),
            kick: AtomicI32::new(0),
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
    /// Capture to bitstream collected for the last picture, in microseconds.
    ///
    /// **A property of the stream, not of a guest**: one encode serves every
    /// guest, so they all wait the same time for it. Each guest folds it into
    /// its own smoothed figure, because the cadence that reports it is per
    /// guest.
    encode_us: AtomicU32,
    /// The last published stage report, so a caller can read the numbers
    /// without reaching into the thread that produces them.
    timing: TimingCells,
    /// Raised to end the loop; the loop checks it once per frame.
    stopping: AtomicU32,
    /// Counts encoder reinitializations.
    ///
    /// **A guest has to know, and cannot infer it.** A new encoder is a new
    /// reference chain and a new set of parameter sets, which the peer learns
    /// from the generation in the video header; a guest that never noticed
    /// would keep announcing the old one.
    epoch: AtomicU32,
}

/// A stage report, in microseconds, published as plain atomics.
///
/// Twelve independent stores rather than one snapshot, which means a reader
/// can catch a report half written. That is deliberate: the alternative is a
/// lock on the path that produces frames, and the cost of a torn report is a
/// percentile from two adjacent reports rather than one, in a figure that is
/// already a rolling estimate.
#[derive(Debug, Default)]
struct TimingCells {
    cells: [AtomicU32; 12],
}

impl TimingCells {
    fn publish(&self, report: &Report) {
        let values = [
            report.acquire.p50,
            report.acquire.p95,
            report.acquire.p99,
            report.encode.p50,
            report.encode.p95,
            report.encode.p99,
            report.publish.p50,
            report.publish.p95,
            report.publish.p99,
            report.interval.p50,
            report.interval.p95,
            report.interval.p99,
        ];
        for (cell, value) in self.cells.iter().zip(values) {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a stage duration clamped to the range before conversion"
            )]
            cell.store(
                (value * 1000.0).clamp(0.0, f64::from(u32::MAX)) as u32,
                Ordering::Relaxed,
            );
        }
    }

    fn read(&self) -> Report {
        let mut ms = [0.0f64; 12];
        for (at, cell) in self.cells.iter().enumerate() {
            if let Some(slot) = ms.get_mut(at) {
                *slot = f64::from(cell.load(Ordering::Relaxed)) / 1000.0;
            }
        }
        let stage = |at: usize| crate::timing::Percentiles {
            p50: ms.get(at).copied().unwrap_or(0.0),
            p95: ms.get(at + 1).copied().unwrap_or(0.0),
            p99: ms.get(at + 2).copied().unwrap_or(0.0),
            count: 0,
        };
        Report {
            acquire: stage(0),
            encode: stage(3),
            publish: stage(6),
            interval: stage(9),
        }
    }
}

/// Which bitstream the stream produces.
///
/// **One encode serves every guest**, so this is a property of the stream
/// chosen before anyone connects, not something negotiated per guest. A peer
/// that cannot decode it has to be refused rather than accommodated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// Which encoder produces it.
///
/// Both implement the same trait and one loop drives either. The choice is a
/// deployment one: what the machine has, and which vendor's driver is less
/// unhappy on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// The open stack, through the render node.
    Open,
    /// The vendor's encoder, through its own runtime.
    Vendor,
}

/// How the stream is configured. Fixed for its lifetime.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    pub codec: Codec,
    pub backend: Backend,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// What the operator asked for, before it is divided among guests.
    pub configured_mbps: f64,
    /// The floor a controller may not descend below.
    pub min_mbps: f64,
    /// How the display this stream shows is oriented.
    ///
    /// **The coded picture never rotates.** A quarter turn changes what a peer
    /// presents and what it maps pointer coordinates against, and leaves the
    /// bitstream landscape, so this travels in the header rather than through
    /// the encoder.
    pub rotation: lowlat_core::video::Rotation,
    /// Rows of unpredictable detail the source paints, from the top.
    ///
    /// **Zero is the flat picture every recorded measurement was taken
    /// against.** A nonzero band makes frames large enough to need more than
    /// one fragment, which is the only way the fragmenting path here and the
    /// reassembly a peer runs are exercised together.
    pub detail_rows: u32,
    /// Capture the real display rather than generating pictures.
    ///
    /// **Not a preference between two sources of equal standing.** The
    /// generator exists so everything above it can be built and measured
    /// without a screen; a display is what the product streams. Which node
    /// that is is not configured, because it is discovered: see
    /// [`crate::display::Display::open`].
    pub display: bool,
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
            encode_us: AtomicU32::new(0),
            timing: TimingCells::default(),
            stopping: AtomicU32::new(0),
            epoch: AtomicU32::new(0),
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

    /// The last stage report the loop published.
    ///
    /// Zero until the first report is due, which is what makes a reader that
    /// checks the count rather than the values necessary.
    pub fn timings(&self) -> Report {
        self.shared.timing.read()
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
                seat.refresh.store(0, Ordering::Relaxed);
                // **The declaration belongs to the guest, not to the seat.**
                // Left standing, an arriving guest would be counted in the
                // consensus as whatever the last occupant could decode, until
                // its own initialization landed.
                seat.flags.store(0, Ordering::Relaxed);
                seat.reconfigure.store(0, Ordering::Relaxed);
                seat.asked_last.store(0, Ordering::Relaxed);
                seat.kick.store(0, Ordering::Relaxed);
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

    /// Capture to bitstream collected for the last picture, in milliseconds.
    ///
    /// What the encode-latency message carries, once a guest has smoothed it.
    pub fn encode_latency_ms(&self) -> f64 {
        f64::from(self.shared.encode_us.load(Ordering::Relaxed)) / 1000.0
    }

    /// This guest asked for a refresh.
    ///
    /// Taken by the loop on its next frame. It goes through the gate's
    /// throttle like every other request, so a peer failing on every frame
    /// cannot turn its own recovery into a refresh per frame.
    pub fn request_refresh(&self) {
        if let Some(seat) = self.shared.seats.get(self.index) {
            seat.refresh.store(1, Ordering::Relaxed);
        }
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

    /// What this guest can decode, as the video flag bits.
    ///
    /// Published on every change rather than once, because the declaration
    /// arrives in two places and the second is how a peer changes its mind.
    pub fn declare(&self, flags: u32) {
        if let Some(seat) = self.shared.seats.get(self.index) {
            seat.flags.store(flags, Ordering::Relaxed);
        }
    }

    /// This guest asked for the encoder to be reinitialized.
    ///
    /// **Declare first.** The loop reads every seat's flags when it takes this
    /// request, so a request that arrives before the flags it is asking for
    /// would be answered against the old ones.
    pub fn request_reconfigure(&self) {
        if let Some(seat) = self.shared.seats.get(self.index) {
            seat.reconfigure.store(1, Ordering::Release);
        }
    }

    /// Why this guest is being ended, if it is.
    ///
    /// Set by the loop, read by the guest, and the guest is what turns it into
    /// a message: the loop has no session to write to and the guest has no way
    /// of knowing the encoder is gone.
    pub fn kicked(&self) -> Option<i32> {
        let reason = self
            .shared
            .seats
            .get(self.index)?
            .kick
            .load(Ordering::Acquire);
        (reason != 0).then_some(reason)
    }

    /// How many times the stream's encoder has been reinitialized.
    ///
    /// A guest compares this against what it last saw; a change means the
    /// reference chain restarted and a new generation has to be announced.
    pub fn epoch(&self) -> u32 {
        self.shared.epoch.load(Ordering::Acquire)
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

/// The seated guests, and the per-guest state that goes with them.
///
/// **Owned above the encoder, deliberately.** An encoder rebuilt mid-session
/// must not take the seated guests with it: a seat is announced exactly once,
/// when the guest claims it, so a loop that started fresh would never hear
/// about a guest that was already streaming and would publish to nobody.
#[derive(Default)]
struct Roster {
    active: Vec<Active>,
    guests: Vec<gate::Guest>,
    controllers: Vec<Controller>,
    /// **The delivery gate is the session's, not the encoder's.** It carries
    /// the largest frame the session has produced, and a latched guest is
    /// retested against that rather than against the frame in hand. Rebuilt
    /// with the encoder it starts at nothing, so every guest the rebuild
    /// latched is re-admitted on the next frame whatever its size -- which is
    /// the keyframe that does not fit, the grant spent, and the spike paid for
    /// a recovery that did not happen.
    gate: Gate,
}

/// The loop.
fn run(shared: &Shared, arrivals: &mpsc::Receiver<Join>, mut config: Config) {
    // **The construction differs and the loop does not.** Each backend owns a
    // device, a context and an encoder whose lifetimes nest, so they are built
    // here and the same generic loop is handed whichever one exists.
    //
    // **And it is built more than once.** A peer changes what it can decode
    // with a message rather than by reconnecting, and a different codec is a
    // different encoder, so the loop can hand this one back and ask for
    // another. The guests outlive that; see [`Roster`].
    let mut roster = Roster::default();
    let mut previous: Option<Codec> = None;
    loop {
        // Waiting rather than holding hardware. A host advertises itself long
        // before anyone connects, and an encoder open across that whole time
        // is a device another application cannot have.
        //
        // **Waited for on every pass, not once.** A build that fails ends the
        // guests that were waiting on it, and the next guest to arrive is owed
        // the same attempt: a device busy a moment ago may not be, and one
        // that is still busy has to say so again rather than leaving the
        // arrival connected to a thread that gave up before it got here.
        loop {
            // **The waiting loop has to retire seats too.** A guest that has
            // been told the session is over gives its seat back, and only this
            // thread may empty one; a wait that just tested occupancy would
            // see the leaving seat, call it occupied, and rebuild the encoder
            // that had just failed, at whatever rate the device refuses it.
            retire_leaving(shared);
            if occupied(shared) {
                break;
            }
            if shared.stopping.load(Ordering::Acquire) != 0 {
                return;
            }
            std::thread::sleep(IDLE_WAIT);
        }

        lowlat_common::log_info!(
            "stream: encoding w={} h={} fps={} ceiling_mbps={:.1} codec={:?} backend={:?}",
            config.width,
            config.height,
            config.fps,
            config.configured_mbps,
            config.codec,
            config.backend
        );

        let exit = match config.backend {
            Backend::Open => run_open(shared, arrivals, config, &mut roster),
            Backend::Vendor => run_vendor(shared, arrivals, config, &mut roster),
        };
        match exit {
            Exit::Stopped => return,
            // **A codec the device refuses is not the end of the stream, but
            // it is the end for whoever asked.** The encoder that was running
            // a moment ago worked, so the guests that were watching keep their
            // picture; the guest that asked has already rebuilt its decoder for
            // a stream it is never going to receive, and is told so.
            Exit::Failed(reason) => {
                let Some(back) = previous.take() else {
                    lowlat_common::log_error!(
                        "stream: no encoder for codec={:?}, ending {} guest(s), reason={}",
                        config.codec,
                        occupied_seats(shared),
                        reason
                    );
                    kick_all(shared, &roster.active, reason);
                    // Back to waiting rather than out. The seats free as their
                    // guests read the reason, and the next arrival gets its own
                    // attempt at the device.
                    //
                    // **Paced, because the guests do not leave instantly.**
                    // They have to notice, say goodbye and be retired, and
                    // rebuilding on every pass until then asks a device that
                    // just refused for an encoder thousands of times a second.
                    roster = Roster::default();
                    std::thread::sleep(IDLE_WAIT);
                    continue;
                };
                lowlat_common::log_warn!(
                    "stream: codec={:?} could not be configured (reason={}), staying on {:?}",
                    config.codec,
                    reason,
                    back
                );
                kick_asked(shared, &roster.active, reason);
                config.codec = back;
            }
            Exit::Reconfigure(codec) => {
                lowlat_common::log_info!(
                    "stream: reconfiguring codec={:?} -> {:?}",
                    config.codec,
                    codec
                );
                previous = Some(config.codec);
                config.codec = codec;
                // **Every guest is latched and the generation moves.** The new
                // encoder has no history at all, so a guest still expecting the
                // old reference chain waits for the refresh that opens the new
                // one, and its peer is told the chain restarted rather than
                // being left to infer it.
                for guest in &mut roster.guests {
                    guest.mark_skipping();
                }
                shared.epoch.fetch_add(1, Ordering::Release);
            }
        }
    }
}

/// Why the encode loop handed the encoder back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    /// The host is stopping.
    Stopped,
    /// The encoder could not be built, or stopped working, and the guests are
    /// owed the reason.
    Failed(i32),
    /// Every seated guest can decode this codec, and at least one asked for a
    /// configuration the running encoder does not produce.
    Reconfigure(Codec),
}

/// End the guests that asked for the configuration that could not be built.
///
/// **Only they are owed this.** A peer rebuilds its decoder the moment it asks
/// rather than waiting to be told the request was granted, so a guest whose
/// request failed is holding a decoder for a stream that will never arrive.
/// The guests that asked for nothing are still watching the encoder that
/// worked a moment ago.
fn kick_asked(shared: &Shared, active: &[Active], reason: i32) {
    for entry in active {
        if let Some(seat) = shared.seats.get(entry.seat)
            && seat.asked_last.swap(0, Ordering::AcqRel) != 0
        {
            seat.kick.store(reason, Ordering::Release);
            let _ = entry.wake.notify();
        }
    }
}

/// End every seated guest, with a reason.
///
/// **The loop cannot send anything itself.** It owns no session; each guest
/// owns its own and is the only thing that can write to its peer. So the
/// reason is left on the seat and the guest turns it into a message.
fn kick_all(shared: &Shared, active: &[Active], reason: i32) {
    // **Every occupied seat, not only the ones the loop knows about.** A seat
    // is promoted into the roster on the loop's first pass, so a build that
    // fails before that has a guest holding a seat and no entry to find it by,
    // and it was left connected to nothing until its own deadline noticed.
    for seat in &shared.seats {
        if seat.state.load(Ordering::Acquire) != seat_state::FREE {
            seat.kick.store(reason, Ordering::Release);
        }
    }
    // The wake is an optimisation over the guest's own polling, and only the
    // seats the loop has admitted have a handle to wake by.
    for entry in active {
        let _ = entry.wake.notify();
    }
}

/// Give back the seats whose guests have left.
///
/// **Only this thread may**, which is the seat protocol's whole point: the
/// loop is the only pusher, so it is the only thing that can know nothing more
/// is going into a ring. See the module note.
fn retire_leaving(shared: &Shared) {
    for seat in &shared.seats {
        if seat.state.load(Ordering::Acquire) == seat_state::LEAVING {
            release(seat, &shared.pool);
            seat.state.store(seat_state::FREE, Ordering::Release);
        }
    }
}

/// Bind the rate and the gate to the number of guests sharing the stream.
///
/// **The gate's ceiling is the divided rate, not the configured one.** A
/// guest's room test scales with what that guest is allowed to send, so a
/// second guest halves both the rate and the window it is measured against.
fn rebind(
    budget: &mut Budget,
    guests_seated: usize,
    guests: &mut [gate::Guest],
    controllers: &mut [Controller],
) {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "bounded by MAX_SEATS, which is sixteen"
    )]
    budget.rebound(guests_seated as u32, controllers);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "a rate in megabits, only used to pick one of three ceiling steps"
    )]
    let ceiling = budget.ceiling() as f32;
    for guest in guests.iter_mut() {
        guest.set_rate(ceiling);
    }
}

/// Seats with a guest on them, admitted or not.
fn occupied_seats(shared: &Shared) -> usize {
    shared
        .seats
        .iter()
        .filter(|seat| seat.state.load(Ordering::Acquire) != seat_state::FREE)
        .count()
}

/// Capability bits a peer can declare that this pipeline does not emit.
///
/// Four-four-four chroma and ten-bit colour are reserved and unimplemented
/// (docs/00-overview.md D7), so a request for either is read and reported
/// rather than quietly treated as granted, which would leave the peer building
/// a decoder for a stream it will never receive.
///
/// **The base flag is not a capability and is not listed here.** It is set on
/// every declaration and means nothing; testing it as one reports a refusal on
/// every ordinary request, which is what it did.
const NOT_EMITTED: u32 = lowlat_core::init::FLAG_COLOR444 | lowlat_core::init::FLAG_10BIT;

fn run_open(
    shared: &Shared,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    let (codec, params) = match config.codec {
        Codec::H264 => (
            lowlat_encode::vaapi::Codec::H264,
            lowlat_encode::vaapi::Params::H264(lowlat_encode::h264::Params {
                width: config.width,
                height: config.height,
                fps: config.fps,
                level_idc: H264_LEVEL,
                log2_max_frame_num_minus4: 4,
                log2_max_poc_lsb_minus4: 4,
                max_num_ref_frames: 1,
            }),
        ),
        Codec::H265 => (
            lowlat_encode::vaapi::Codec::H265,
            lowlat_encode::vaapi::Params::H265(lowlat_encode::h265::Params {
                width: config.width,
                height: config.height,
                fps: config.fps,
                level_idc: H265_LEVEL,
                log2_max_poc_lsb_minus4: 4,
                max_num_ref_frames: 1,
            }),
        ),
    };
    let Ok(display) = lowlat_encode::vaapi::Vaapi::load() else {
        lowlat_common::log_error!("stream: display runtime unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(display) = display.open(c"/dev/dri/renderD128") else {
        lowlat_common::log_error!("stream: render node could not be opened");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(caps) = display.caps(codec) else {
        lowlat_common::log_error!("stream: render node reports no encode for codec={codec:?}");
        return Exit::Failed(status::ENCODER_CAPABILITIES);
    };
    let Ok(context) = display.create_context(caps, config.width, config.height, ENCODE_DEPTH)
    else {
        lowlat_common::log_error!("stream: encode context could not be created");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(mut encoder) = context.encoder(params, start_bps(config)) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **No display source here.** The conversion runs on the device the display
    // is attached to, and a frame on one card cannot reach an encoder on
    // another without the copy through system memory this path exists to
    // avoid. When this backend can take a frame by descriptor, the two meet.
    encode_loop(shared, arrivals, config, roster, &mut encoder, None)
}

fn run_vendor(
    shared: &Shared,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    let Ok(cuda) = lowlat_encode::cuda::Cuda::load() else {
        lowlat_common::log_error!("stream: compute runtime unavailable, nothing will encode");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(device) = cuda.any_device() else {
        lowlat_common::log_error!("stream: no compute device");
        return Exit::Failed(status::ENCODER_CAPABILITIES);
    };
    let Ok(compute) = cuda.retain_primary(&device) else {
        lowlat_common::log_error!("stream: compute context could not be retained");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(api) = lowlat_encode::nvenc::Api::load() else {
        lowlat_common::log_error!("stream: encoder runtime unavailable");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let Ok(session) = api.open_session(compute) else {
        lowlat_common::log_error!("stream: encode session could not be opened");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    // **The display decides the picture size when it is the source.** The
    // encoder is created before the source exists and its registration fixes
    // the shape, so asking the display first is the only way the two agree.
    let (width, height) = if config.display {
        // **A display that is asleep is the ordinary case for this product,
        // not a fault.** Somebody connecting to a machine whose screen has
        // powered down is most of what remote access is for, so it is waited
        // for rather than refused. The wait is bounded because the only thing
        // that wakes a blanked display is somebody at the desk, and a guest
        // held indefinitely on a machine nobody is at learns nothing.
        match await_display() {
            Some(size) => {
                if size != (config.width, config.height) {
                    lowlat_common::log_info!(
                        "stream: the display is {}x{}, not the configured {}x{}; following it",
                        size.0,
                        size.1,
                        config.width,
                        config.height
                    );
                }
                size
            }
            None => {
                lowlat_common::log_error!(
                    "stream: nothing has been scanning out for {:.0}s, ending {} guest(s)",
                    DISPLAY_WAIT.as_secs_f64(),
                    roster.active.len()
                );
                return Exit::Failed(status::CAPTURE_UNAVAILABLE);
            }
        }
    } else {
        (config.width, config.height)
    };
    let Ok(mut encoder) = session.initialize(
        &cuda,
        lowlat_encode::nvenc::Config {
            codec: match config.codec {
                Codec::H264 => lowlat_encode::nvenc::Codec::H264,
                Codec::H265 => lowlat_encode::nvenc::Codec::H265,
            },
            width,
            height,
            fps: config.fps,
            bitrate_bps: start_bps(config),
            min_qp: lowlat_encode::nvenc::DEFAULT_MIN_QP,
        },
    ) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let mut desktop = if config.display {
        match crate::display::Display::open(lowlat_encode::nvenc::IN_FLIGHT, &encoder) {
            Ok(desktop) => Some(desktop),
            Err(error) => {
                lowlat_common::log_error!("stream: the display could not be opened, {error}");
                return Exit::Failed(status::ENCODER_UNAVAILABLE);
            }
        }
    } else {
        None
    };
    encode_loop(
        shared,
        arrivals,
        config,
        roster,
        &mut encoder,
        desktop.as_mut(),
    )
}

/// The lowest level each codec has that carries 1080p60, which is 4.2 on the
/// first and 4.1 on the second. **They are written on different scales**: ten
/// times the level number on the first, thirty on the second. Writing the
/// first codec's scale into the second declares level 1.4, which is far below
/// what this resolution needs, and a strict decoder refuses the stream.
const H264_LEVEL: u32 = 42;
const H265_LEVEL: u32 = 123;

/// The rate an encoder opens at, before the controller has said anything.
fn start_bps(config: Config) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a configured bitrate in megabits, well inside u32 as bits per second"
    )]
    {
        (config.configured_mbps * 1_000_000.0) as u32
    }
}

/// How long the loop sleeps while no guest is seated.
const IDLE_WAIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Pictures the encoder may hold at once.
const ENCODE_DEPTH: usize = 4;

/// Collects in a row that must fail before the encoder is called stopped.
///
/// **Not one.** A device can refuse a single collect and answer the next, and
/// ending every guest over one refusal would turn a hiccup into a
/// disconnection. A run of them is a device that is not coming back.
const COLLECT_FAILURES: u32 = 5;

fn occupied(shared: &Shared) -> bool {
    shared
        .seats
        .iter()
        .any(|seat| seat.state.load(Ordering::Acquire) != seat_state::FREE)
}

/// The frame loop, written against the trait rather than a backend, so the
/// second implementation is a construction change and not a second loop.
///
/// **Two deadlines, not one.** A loop that submits a frame and then waits for
/// it is serialised: nothing prepares the next frame while the hardware works,
/// and the pipeline caps at one frame per encode however fast the encoder is.
/// This one asks the encoder for a finished picture on every pass and reaches
/// for a new frame when the frame clock says so, so an encode overlaps the
/// acquire and the submit behind it, and a picture leaves within a poll of
/// being ready rather than at the next frame boundary.
/// How long a guest waits for a display that is not scanning out.
///
/// Long enough that a screen blanked while nobody was looking has a chance to
/// come back when somebody arrives, short enough that a guest on a machine
/// with no display at all is told so rather than held.
const DISPLAY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Wait for something to scan out, and report what shape it is.
fn await_display() -> Option<(u32, u32)> {
    let began = lowlat_common::clock::Time::now();
    let mut said = false;
    loop {
        if let Some(size) = crate::display::Display::size_of_display() {
            return Some(size);
        }
        if !said {
            said = true;
            lowlat_common::log_info!(
                "stream: nothing is scanning out, waiting for a display to come back"
            );
        }
        if lowlat_common::clock::elapsed_ms(began) >= DISPLAY_WAIT.as_secs_f64() * 1000.0 {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Handing over a picture that is already on the device.
///
/// **Host-local and deliberately narrow.** One backend can take a frame this
/// way today and the other cannot, so putting it on the encoder interface would
/// be an abstraction over a single implementation. When the second exists it
/// moves there and this goes away, which is the point at which a type parameter
/// would have earned its place.
trait FromDevice {
    /// True when the picture was queued. False is back pressure or a backend
    /// that does not take frames this way, and the caller treats both the same:
    /// it goes round and tries again.
    fn submit_from_device(
        &mut self,
        input: &lowlat_encode::nvenc::Input,
        force_keyframe: bool,
    ) -> bool;
}

impl FromDevice for lowlat_encode::nvenc::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        input: &lowlat_encode::nvenc::Input,
        force_keyframe: bool,
    ) -> bool {
        self.submit_registered(input, force_keyframe).is_ok()
    }
}

#[cfg(test)]
impl FromDevice for tests::Fake {
    fn submit_from_device(
        &mut self,
        _input: &lowlat_encode::nvenc::Input,
        _force_keyframe: bool,
    ) -> bool {
        false
    }
}

impl FromDevice for lowlat_encode::vaapi::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        _input: &lowlat_encode::nvenc::Input,
        _force_keyframe: bool,
    ) -> bool {
        false
    }
}

fn encode_loop<E: Encoder + FromDevice>(
    shared: &Shared,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    encoder: &mut E,
    display: Option<&mut crate::display::Display>,
) -> Exit {
    let mut display = display;
    // **The block says which codec is on the wire**, so a live run is
    // identifiable from the picture rather than from a log on the other
    // machine. Nothing downstream reads it; it is for the person watching.
    let marker = match config.codec {
        Codec::H264 => Marker::BLUE,
        Codec::H265 => Marker::GREEN,
    };
    let mut source =
        Synthetic::with_detail(config.width, config.height, config.detail_rows).with_marker(marker);
    let mut budget = Budget::new(config.configured_mbps, config.min_mbps);
    let mut stages = Stages::default();

    // Compact and parallel: one entry per streaming guest, in no particular
    // order. Compaction happens on join and leave, which are rare, so the
    // per-frame work is a walk over exactly the guests that exist. They are
    // lent from above rather than owned here; see [`Roster`].
    let Roster {
        active,
        guests,
        controllers,
        gate,
    } = roster;
    let mut samples: Vec<Sample> = Vec::with_capacity(MAX_SEATS);
    // Reaching here means an encoder exists, so whatever was asked for was
    // granted and nobody is owed a refusal for it.
    for entry in active.iter() {
        if let Some(seat) = shared.seats.get(entry.seat) {
            seat.asked_last.store(0, Ordering::Release);
        }
    }
    // **The budget is new and the guests are not.** It is rebound when a
    // guest joins or leaves, and a rebuilt encoder is neither, so it would
    // start again at a count of none and read as undivided. Nothing downstream
    // reads that count today -- the controllers and the gate ceilings ride in
    // the roster with their divided bounds intact -- so this states the
    // invariant rather than repairing a fault, and keeps the loop from
    // depending on that being true by accident.
    rebind(&mut budget, active.len(), guests, controllers);
    // When each picture still inside the encoder was captured and submitted,
    // oldest first. **A stamp per picture, not one for the loop**: with more
    // than one in flight, the picture that comes back is not the one that
    // went in last, and a single stamp would report the wrong frame's latency
    // for every frame after the first.
    let mut in_flight: std::collections::VecDeque<(
        lowlat_common::clock::Time,
        lowlat_common::clock::Time,
    )> = std::collections::VecDeque::with_capacity(ENCODE_DEPTH + 1);

    let started = lowlat_common::clock::Time::now();
    let interval_ms = 1000.0 / f64::from(config.fps.max(1));
    let mut next_frame_ms = 0.0f64;
    let mut previous_submit: Option<lowlat_common::clock::Time> = None;
    let mut force_keyframe = false;
    let mut since_report = 0u32;
    let mut failures = 0u32;
    // Whether the last pass found the display dark.
    let mut held = false;

    loop {
        if shared.stopping.load(Ordering::Acquire) != 0 {
            return Exit::Stopped;
        }

        let moved = admit_and_retire(shared, arrivals, config, active, guests, controllers);
        if moved {
            rebind(&mut budget, active.len(), guests, controllers);
        }
        if active.is_empty() {
            std::thread::sleep(IDLE_WAIT);
            next_frame_ms = lowlat_common::clock::elapsed_ms(started);
            continue;
        }

        // **A peer changes what it can decode with a message, not by
        // reconnecting**, so the answer is a different encoder rather than a
        // different session. Taken outside the frame clock: a reconfiguration
        // is not worth a frame's wait and the encoder is about to be replaced
        // anyway.
        if reconfigure_asked(shared, active) {
            let asked = consensus(shared, active);
            if asked & NOT_EMITTED != 0 {
                lowlat_common::log_warn!(
                    "stream: guests asked for flags={:#x} and this pipeline emits 8-bit 4:2:0, \
                     so {:#x} of it is not granted",
                    asked,
                    asked & NOT_EMITTED
                );
            }
            let wanted = if asked & lowlat_core::init::FLAG_HEVC != 0 {
                Codec::H265
            } else {
                Codec::H264
            };
            // **The whole decision on one line.** What every seat agreed on,
            // how many seats that was over, and what it settles the codec at:
            // a request that changes nothing and a request that is outvoted
            // look identical from the outside otherwise.
            lowlat_common::log_info!(
                "stream: reinit asked, consensus={:#x} over {} seat(s), codec={:?} -> {:?}",
                asked,
                active.len(),
                config.codec,
                wanted
            );
            if wanted != config.codec {
                return Exit::Reconfigure(wanted);
            }
            // Nothing here this encoder does not already produce, so what the
            // request is owed is what a reinitialization would have given it:
            // a picture with no history behind it.
            force_keyframe = true;
        }

        // Anything the hardware finished while this loop was elsewhere. Never
        // waits: a picture that is not ready costs a driver round trip and the
        // loop goes on to the frame clock.
        let collected = collect(
            shared,
            encoder,
            gate,
            active,
            guests,
            &mut in_flight,
            &mut stages,
            started,
        );
        force_keyframe |= collected.keyframe;
        // **A stopped encoder used to be a log line per pass, forever.** The
        // guests went on holding seats and receiving nothing, and the only
        // thing that eventually noticed was each peer's own liveness deadline.
        failures = if collected.failed { failures + 1 } else { 0 };
        if failures >= COLLECT_FAILURES {
            lowlat_common::log_error!(
                "stream: encoder stopped answering after {failures} attempts, ending {} guest(s)",
                active.len()
            );
            return Exit::Failed(status::ENCODE_FAILED);
        }

        let now_ms = lowlat_common::clock::elapsed_ms(started);
        if now_ms >= next_frame_ms {
            // The deadline advances by the interval rather than from now, so
            // the frame clock does not drift, and it is pulled forward after a
            // stall rather than catching up in a burst nobody can use.
            next_frame_ms = if next_frame_ms + interval_ms < now_ms {
                now_ms + interval_ms
            } else {
                next_frame_ms + interval_ms
            };

            let began = lowlat_common::clock::Time::now();
            // **The desktop when there is one, and the generator otherwise.**
            // A picture from the display is already on the device and is
            // handed over by reference; the generator's is bytes and is
            // uploaded. Nothing else in this loop can tell them apart.
            // **A display that cannot be read holds its last picture; it never
            // falls back to the generator.** The two are not interchangeable
            // sources: one is the machine and the other is a test pattern, and
            // a guest whose screen turned into moving colour bars would have
            // no way to tell that from the host having gone wrong.
            let ready = match display.as_deref_mut() {
                Some(desktop) => {
                    let read = desktop.acquire();
                    let dark = read.is_err();
                    if dark != held {
                        held = dark;
                        // Once per transition. Per frame it is sixty lines a
                        // second saying the same thing.
                        match &read {
                            Err(error) => lowlat_common::log_warn!(
                                "stream: the display stopped, holding the last picture, {error}"
                            ),
                            Ok(_) => lowlat_common::log_info!("stream: the display came back"),
                        }
                    }
                    Some(read.unwrap_or(began))
                }
                None => None,
            };
            let synthetic = if display.is_none() {
                Some(source.acquire())
            } else {
                None
            };
            let captured_at = ready
                .or_else(|| synthetic.as_ref().map(|frame| frame.captured_at))
                .unwrap_or(began);
            stages
                .acquire
                .record(lowlat_common::clock::diff_ms(began, captured_at));

            // **The tick is the frame.** The controller counts its periods in
            // ticks, so this belongs here and not on the poll pass.
            tick_rate(
                shared,
                active,
                guests,
                controllers,
                &mut samples,
                &mut budget,
                encoder,
            );

            // **A peer that cannot decode is the only party that knows.** Its
            // decoder has failed on something the wire delivered intact, and
            // the only recovery is a picture with no history behind it.
            // Throttled like every other request, so a peer failing on every
            // frame cannot ask for one per frame.
            if refresh_asked(shared, active) {
                let asked_at = lowlat_common::clock::elapsed_ms(started);
                force_keyframe |= gate.request_keyframe(asked_at) == Keyframe::Request;
            }

            let submitted_at = lowlat_common::clock::Time::now();
            let queued = match (&synthetic, display.as_deref_mut()) {
                (Some(frame), _) => encoder.submit(frame, force_keyframe).is_ok(),
                (None, Some(desktop)) => match desktop.presented() {
                    Some(input) => encoder.submit_from_device(input, force_keyframe),
                    None => false,
                },
                (None, None) => false,
            };
            if queued {
                force_keyframe = false;
                if let Some(previous) = previous_submit {
                    stages
                        .interval
                        .record(lowlat_common::clock::diff_ms(previous, submitted_at));
                }
                previous_submit = Some(submitted_at);
                in_flight.push_back((captured_at, submitted_at));

                since_report += 1;
                if since_report >= REPORT_FRAMES {
                    since_report = 0;
                    shared.timing.publish(&stages.report());
                }
            }
            // A refusal is the encoder holding as many pictures as it will:
            // back pressure rather than a fault, and the next frame goes in
            // once a collect has made room.
        }

        // Wait for the sooner of the next frame and the next poll. **A
        // millisecond is the floor**, because anything shorter asked of the
        // scheduler is a busy wait with extra steps; the final approach to a
        // frame deadline is the one place an accurate landing is worth its
        // spin, and it happens once per frame rather than once per poll.
        let remaining = next_frame_ms - lowlat_common::clock::elapsed_ms(started);
        if remaining > POLL_MS * 2.0 {
            std::thread::sleep(COLLECT_WAIT);
        } else if remaining > 0.0 {
            lowlat_common::clock::precise_sleep(std::time::Duration::from_secs_f64(
                remaining / 1000.0,
            ));
        }
    }
}

/// One pass of the controller over every guest, and the reconfigure it asks
/// for.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site, lifted out of the loop for legibility alone"
)]
fn tick_rate<E: Encoder>(
    shared: &Shared,
    active: &[Active],
    guests: &mut [gate::Guest],
    controllers: &mut [Controller],
    samples: &mut Vec<Sample>,
    budget: &mut Budget,
    encoder: &mut E,
) {
    samples.clear();
    for entry in active {
        let Some(seat) = shared.seats.get(entry.seat) else {
            continue;
        };
        samples.push(Sample {
            window: seat.window.load(Ordering::Relaxed),
            stale: seat.stale.load(Ordering::Relaxed),
            measured_mbps: f64::from(f32::from_bits(seat.measured_bits.load(Ordering::Relaxed))),
        });
    }
    for ((guest, sample), entry) in guests.iter_mut().zip(samples.iter()).zip(active.iter()) {
        guest.set_outstanding(sample.window);
        // A frame the transport refused after the gate admitted it. The count
        // is taken rather than read, so one loss latches once.
        if let Some(seat) = shared.seats.get(entry.seat)
            && seat.missed.swap(0, Ordering::Relaxed) != 0
        {
            guest.mark_skipping();
        }
    }
    if let Some(rate_mbps) = budget.tick(controllers, samples) {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a rate the controller has already clamped to its bounds"
        )]
        let bps = (rate_mbps * 1_000_000.0) as u32;
        // A live change. It reinitialises nothing and forces no refresh, which
        // is what keeps the stream unbroken across a reconfigure.
        let _ = encoder.reconfigure(bps);
    }
}

/// Whether any guest has asked for the encoder to be reinitialized, clearing
/// the requests.
fn reconfigure_asked(shared: &Shared, active: &[Active]) -> bool {
    let mut asked = false;
    for entry in active {
        if let Some(seat) = shared.seats.get(entry.seat) {
            let this = seat.reconfigure.swap(0, Ordering::AcqRel) != 0;
            // Remembered rather than only counted, so a build that fails can
            // be reported to whoever asked for it.
            if this {
                seat.asked_last.store(1, Ordering::Release);
            }
            asked |= this;
        }
    }
    asked
}

/// What every seated guest can decode, as the video flag bits.
///
/// **The intersection, not the last request.** One encode serves every seat
/// (docs/00-overview.md D11), so a capability only some of them declare is one
/// none of them can be sent: granting it would hand the others a stream their
/// decoders were not built for, and they report that as a decode failure
/// rather than as a mismatch. With a single seat the intersection is exactly
/// what that seat asked for, which is the ordinary case.
///
/// No seats is no capability, rather than every capability. An empty
/// intersection is vacuously everything, and acting on that would configure a
/// stream from nobody's declaration at all.
fn consensus(shared: &Shared, active: &[Active]) -> u32 {
    let mut flags: Option<u32> = None;
    for entry in active {
        if let Some(seat) = shared.seats.get(entry.seat) {
            let declared = seat.flags.load(Ordering::Relaxed);
            flags = Some(flags.map_or(declared, |all| all & declared));
        }
    }
    flags.unwrap_or(0)
}

/// Whether any guest has asked for a refresh, clearing the requests.
fn refresh_asked(shared: &Shared, active: &[Active]) -> bool {
    let mut asked = false;
    for entry in active {
        if let Some(seat) = shared.seats.get(entry.seat) {
            asked |= seat.refresh.swap(0, Ordering::Relaxed) != 0;
        }
    }
    asked
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

/// What one pass of collecting found.
#[derive(Debug, Clone, Copy)]
struct Collected {
    /// A guest is latched and wants a keyframe.
    keyframe: bool,
    /// The encoder refused to answer.
    ///
    /// **One of these is not a fault.** A device can refuse a single collect
    /// for reasons that clear on the next pass, so what says an encoder has
    /// stopped is a run of them, and the run is what the loop counts.
    failed: bool,
}

/// Take whatever the encoder has finished, and deliver each picture.
///
/// Returns whether a keyframe was asked for. **Never waits.** A picture that
/// is not ready costs a driver round trip and the caller goes on to its frame
/// clock; waiting here is what would serialise the pipeline.
#[allow(
    clippy::too_many_arguments,
    reason = "one call site; a struct here would only rename the arguments"
)]
fn collect<E: Encoder>(
    shared: &Shared,
    encoder: &mut E,
    gate: &mut Gate,
    active: &[Active],
    guests: &mut [gate::Guest],
    in_flight: &mut std::collections::VecDeque<(
        lowlat_common::clock::Time,
        lowlat_common::clock::Time,
    )>,
    stages: &mut Stages,
    started: lowlat_common::clock::Time,
) -> Collected {
    let mut wanted = false;
    loop {
        match encoder.poll() {
            Err(_) => {
                lowlat_common::log_error!("stream: collect failed");
                return Collected {
                    keyframe: wanted,
                    failed: true,
                };
            }
            Ok(Poll::Pending) => {
                return Collected {
                    keyframe: wanted,
                    failed: false,
                };
            }
            Ok(Poll::Ready {
                bitstream,
                keyframe,
            }) => {
                let len = bitstream.len();
                let collected_at = lowlat_common::clock::Time::now();
                // The oldest picture in the encoder is the one that came back.
                let (captured_at, submitted_at) = in_flight
                    .pop_front()
                    .unwrap_or((collected_at, collected_at));
                stages
                    .encode
                    .record(lowlat_common::clock::diff_ms(submitted_at, collected_at));
                // Stamped against the capture, which is what every latency
                // figure in docs/05-host.md section 10 is measured from. A
                // stage that restamps it destroys the measurement rather than
                // merely losing it.
                let elapsed_us = lowlat_common::clock::diff_ms(captured_at, collected_at) * 1000.0;
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "clamped to the range before the conversion"
                )]
                shared.encode_us.store(
                    elapsed_us.clamp(0.0, f64::from(u32::MAX)) as u32,
                    Ordering::Relaxed,
                );

                let now_ms = lowlat_common::clock::elapsed_ms(started);
                let Some(mut writer) = shared.pool.acquire() else {
                    // Every slot is still held, so every guest is behind. The
                    // frame is dropped for all of them, and they are latched:
                    // a dropped frame breaks the reference chain whatever the
                    // reason for dropping it.
                    //
                    // **And the refresh is asked for here**, because the pass
                    // that would otherwise ask is the one that just failed to
                    // take a slot.
                    for guest in guests.iter_mut() {
                        guest.mark_skipping();
                    }
                    wanted |= gate.request_keyframe(now_ms) == Keyframe::Request;
                    return Collected {
                        keyframe: wanted,
                        failed: false,
                    };
                };
                if !writer.fill(bitstream) {
                    // A sizing error rather than a transient one: the slot is
                    // as wide as a deliverable frame can be, so a frame past
                    // it is one no window could have carried either.
                    lowlat_common::log_error!(
                        "stream: coded frame exceeds the pool slot, bytes={len}"
                    );
                    for guest in guests.iter_mut() {
                        guest.mark_skipping();
                    }
                    wanted |= gate.request_keyframe(now_ms) == Keyframe::Request;
                    return Collected {
                        keyframe: wanted,
                        failed: false,
                    };
                }

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
                stages.publish.record(lowlat_common::clock::diff_ms(
                    collected_at,
                    lowlat_common::clock::Time::now(),
                ));
            }
        }
    }
}

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
    pub(super) struct Fake {
        unit: Vec<u8>,
        /// One entry per picture in flight, true where a refresh was asked
        /// for. **A fake that never answers a refresh request would hold every
        /// guest latched forever**, which looks exactly like the loop not
        /// running, so the answer is part of what makes the harness honest.
        queued: std::collections::VecDeque<bool>,
        /// Refresh requests seen, which is how the recovery path is observed
        /// from outside the gate.
        forced: Arc<AtomicU32>,
        /// Collects still to be refused, so a device that stops answering can
        /// be played back a chosen number of times.
        refuse: u32,
        /// Takes frames and produces none, which is how a second run of the
        /// loop can be made to change nothing it inherited.
        mute: bool,
    }

    #[derive(Debug)]
    pub(super) struct Never;

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
            if self.mute {
                return Ok(Poll::Pending);
            }
            if self.refuse > 0 {
                self.refuse -= 1;
                return Err(Never);
            }
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
                display: false,
                width: 320,
                height: 240,
                fps: 240,
                codec: Codec::H264,
                backend: Backend::Open,
                configured_mbps: 10.0,
                min_mbps: 1.0,
                rotation: lowlat_core::video::Rotation::None,
                detail_rows: 0,
            };
            let shared = Arc::new(Shared {
                seats: core::array::from_fn(|_| Seat::new()),
                pool: Pool::new(slots, max_frame_bytes()),
                encode_us: AtomicU32::new(0),
                timing: TimingCells::default(),
                stopping: AtomicU32::new(0),
                epoch: AtomicU32::new(0),
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
                        refuse: 0,
                        mute: false,
                    };
                    let mut roster = Roster::default();
                    encode_loop(&owned, &arrivals, config, &mut roster, &mut encoder, None);
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

    /// A stream whose loop this thread drives, so a return can be observed.
    fn parked() -> (Arc<Shared>, Stream, mpsc::Receiver<Join>) {
        let shared = Arc::new(Shared {
            seats: core::array::from_fn(|_| Seat::new()),
            pool: Pool::new(POOL_SLOTS, max_frame_bytes()),
            encode_us: AtomicU32::new(0),
            timing: TimingCells::default(),
            stopping: AtomicU32::new(0),
            epoch: AtomicU32::new(0),
        });
        let (joins, arrivals) = mpsc::channel();
        let stream = Stream {
            shared: Arc::clone(&shared),
            joins: Some(joins),
            thread: None,
        };
        (shared, stream, arrivals)
    }

    fn test_config(codec: Codec) -> Config {
        Config {
            display: false,
            width: 320,
            height: 240,
            fps: 240,
            codec,
            backend: Backend::Open,
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
        }
    }

    fn fake_encoder() -> Fake {
        Fake {
            unit: vec![0x41; 900],
            queued: std::collections::VecDeque::new(),
            forced: Arc::new(AtomicU32::new(0)),
            refuse: 0,
            mute: false,
        }
    }

    fn seat_of(index: usize) -> Active {
        let wake = lowlat_net::Wake::new().expect("wake");
        Active {
            seat: index,
            wake: wake.handle().expect("handle"),
        }
    }

    /// **The intersection, and it has to be.** One encode serves every seat,
    /// so a capability only one of them declared is one none of them can be
    /// sent. Granting it would hand the others a stream their decoders were
    /// not built for, which they report as a decode failure rather than as a
    /// mismatch.
    #[test]
    fn the_consensus_is_what_every_seat_can_decode() {
        const BASE: u32 = lowlat_core::init::FLAG_BASE;
        const HEVC: u32 = lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC;

        let (shared, _stream, _arrivals) = parked();
        let active = [seat_of(0), seat_of(1)];

        shared.seats[0].flags.store(HEVC, Ordering::Relaxed);
        shared.seats[1].flags.store(BASE, Ordering::Relaxed);
        assert_eq!(
            consensus(&shared, &active),
            BASE,
            "one seat's capability was granted on behalf of both"
        );

        shared.seats[1].flags.store(HEVC, Ordering::Relaxed);
        assert_eq!(consensus(&shared, &active), HEVC, "agreement was not seen");

        // Vacuously everything is the wrong answer: it would configure a
        // stream from nobody's declaration at all.
        assert_eq!(consensus(&shared, &[]), 0, "no seats claimed everything");
    }

    /// A seat carries its occupant's declaration and not the last one's.
    #[test]
    fn a_claimed_seat_declares_nothing_until_its_guest_does() {
        let (shared, stream, _arrivals) = parked();
        let held = stream.seats().take(wake_handle()).expect("a seat");
        held.declare(lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC);
        held.request_reconfigure();
        drop(held);

        // The loop frees a leaving seat; do that part by hand.
        shared.seats[0]
            .state
            .store(seat_state::FREE, Ordering::Release);
        let next = stream.seats().take(wake_handle()).expect("a seat");
        assert_eq!(
            shared.seats[0].flags.load(Ordering::Relaxed),
            0,
            "the new guest inherited what the last one could decode"
        );
        assert_eq!(
            shared.seats[0].reconfigure.load(Ordering::Relaxed),
            0,
            "the new guest inherited a request it never made"
        );
        drop(next);
    }

    /// Watch for something the loop does, then stop the loop.
    ///
    /// **The stop is unconditional**, whether or not what was watched for
    /// happened. A watcher that only stops on success turns a failing
    /// assertion into a test that hangs, which reports less and costs more.
    fn watcher(
        shared: &Arc<Shared>,
        mut happened: impl FnMut(&Shared) -> bool + Send + 'static,
    ) -> std::thread::JoinHandle<bool> {
        let shared = Arc::clone(shared);
        std::thread::spawn(move || {
            let began = lowlat_common::clock::Time::now();
            let mut seen = false;
            while lowlat_common::clock::elapsed_ms(began) < 2000.0 {
                if happened(&shared) {
                    seen = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            shared.stopping.store(1, Ordering::Release);
            seen
        })
    }

    fn wake_handle() -> WakeHandle {
        lowlat_net::Wake::new()
            .expect("wake")
            .handle()
            .expect("handle")
    }

    /// **A rebuilt encoder arrives with its guests already seated**, and the
    /// budget that divides the stream between them does not: it is rebound
    /// when a guest joins or leaves, which a rebuild is not, so it starts
    /// again at a count of none and reads as undivided.
    ///
    /// **This is an invariant made explicit rather than a defect repaired.**
    /// Nothing downstream reads the count today: the controllers and the gate
    /// ceilings ride in the roster with their divided bounds intact, and the
    /// next join passes the real count anyway. It is written down because the
    /// loop currently depends on that accident, and a change to how a guest is
    /// admitted would end it silently.
    #[test]
    fn a_rebuilt_encoder_divides_the_stream_between_the_guests_it_inherited() {
        let (shared, stream, arrivals) = parked();
        let first = stream.seats().take(wake_handle()).expect("a seat");
        let second = stream.seats().take(wake_handle()).expect("a seat");
        first.declare(lowlat_core::init::FLAG_BASE);
        second.declare(lowlat_core::init::FLAG_BASE);

        // One pass to admit both, which is the state a rebuild inherits.
        let mut roster = Roster::default();
        let ticker = watcher(&shared, |shared| {
            shared.seats[1].ring.pop().is_some() || shared.seats[0].ring.pop().is_some()
        });
        let config = test_config(Codec::H264);
        encode_loop(
            &shared,
            &arrivals,
            config,
            &mut roster,
            &mut fake_encoder(),
            None,
        );
        ticker.join().expect("ticker");
        assert_eq!(roster.active.len(), 2, "both guests did not seat");
        shared.stopping.store(0, Ordering::Release);

        // What entering the loop again with that roster has to do.
        let mut budget = Budget::new(config.configured_mbps, config.min_mbps);
        assert!(
            (budget.ceiling() - config.configured_mbps).abs() < f64::EPSILON,
            "a fresh budget starts undivided, which is the trap"
        );
        rebind(
            &mut budget,
            roster.active.len(),
            &mut roster.guests,
            &mut roster.controllers,
        );
        assert!(
            (budget.ceiling() - config.configured_mbps / 2.0).abs() < f64::EPSILON,
            "two guests were each handed the whole stream: {}",
            budget.ceiling()
        );
        drop(first);
        drop(second);
    }

    /// **The largest frame the session has produced is the session's, not the
    /// encoder's.** A latched guest is retested against it, so a rebuild that
    /// forgot it re-admits every guest it just latched against a mark of
    /// nothing: the keyframe they need does not fit, the grant is spent, and
    /// the spike is paid for a recovery that did not happen.
    #[test]
    fn a_rebuilt_encoder_keeps_the_largest_frame_the_session_has_seen() {
        let (shared, stream, arrivals) = parked();
        let held = stream.seats().take(wake_handle()).expect("a seat");
        held.declare(lowlat_core::init::FLAG_BASE);

        let mut roster = Roster::default();
        let ticker = watcher(&shared, |shared| shared.seats[0].ring.pop().is_some());
        encode_loop(
            &shared,
            &arrivals,
            test_config(Codec::H264),
            &mut roster,
            &mut fake_encoder(),
            None,
        );
        assert!(ticker.join().expect("ticker"), "nothing was delivered");
        shared.stopping.store(0, Ordering::Release);

        let largest = roster.gate.largest();
        assert!(largest > 0, "the session never recorded a frame size");

        // **A second run that produces nothing at all**, so what the gate
        // holds afterwards is only what survived the rebuild. A run that
        // encoded would re-accumulate a mark and hide the loss.
        let mut mute = fake_encoder();
        mute.mute = true;
        let watchdog = watcher(&shared, |_| false);
        encode_loop(
            &shared,
            &arrivals,
            test_config(Codec::H265),
            &mut roster,
            &mut mute,
            None,
        );
        watchdog.join().expect("watchdog");
        assert_eq!(
            roster.gate.largest(),
            largest,
            "the largest frame the session had seen was rebuilt away with the encoder"
        );
        drop(held);
    }

    /// **The base flag is set on every declaration and means nothing.**
    /// Counting it as a capability this pipeline does not emit reported a
    /// refusal on every ordinary request for the second codec, which is what
    /// it did until a live run showed the line.
    #[test]
    fn the_base_flag_is_not_a_capability_that_can_be_refused() {
        assert_eq!(
            lowlat_core::init::FLAG_BASE & NOT_EMITTED,
            0,
            "the always-set flag is being read as a request"
        );
        let ordinary = lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC;
        assert_eq!(
            ordinary & NOT_EMITTED,
            0,
            "an ordinary request for the second codec reports something ungranted"
        );
        // The two that really are not emitted still are.
        assert_ne!(lowlat_core::init::FLAG_COLOR444 & NOT_EMITTED, 0);
        assert_ne!(lowlat_core::init::FLAG_10BIT & NOT_EMITTED, 0);
    }

    /// **A peer changes what it can decode with a message, not by
    /// reconnecting.** The loop hands the encoder back and names the codec to
    /// build instead.
    #[test]
    fn a_request_for_the_other_codec_hands_the_encoder_back() {
        let (shared, stream, arrivals) = parked();
        let held = stream.seats().take(wake_handle()).expect("a seat");
        held.declare(lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC);
        held.request_reconfigure();

        let mut roster = Roster::default();
        // **A loop that never hands the encoder back would hang here.** The
        // watchdog turns that into a failed assertion, which says what
        // happened; a hung test says only that something did. It waits on the
        // loop rather than on a fixed budget, so it costs nothing when the
        // loop returns as it should.
        let done = Arc::new(AtomicU32::new(0));
        let returned = Arc::clone(&done);
        let guarded = Arc::clone(&shared);
        let watchdog = std::thread::spawn(move || {
            let began = lowlat_common::clock::Time::now();
            while returned.load(Ordering::Acquire) == 0
                && lowlat_common::clock::elapsed_ms(began) < 2000.0
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            guarded.stopping.store(1, Ordering::Release);
        });
        let exit = encode_loop(
            &shared,
            &arrivals,
            test_config(Codec::H264),
            &mut roster,
            &mut fake_encoder(),
            None,
        );
        done.store(1, Ordering::Release);
        watchdog.join().expect("watchdog");
        shared.stopping.store(0, Ordering::Release);
        assert_eq!(
            exit,
            Exit::Reconfigure(Codec::H265),
            "the request for the other codec was not answered with one"
        );

        // **The guests outlive the encoder, and the proof is the second
        // run.** A seat is announced exactly once, when the guest claims it,
        // so nothing arrives on the channel this time: a loop that rebuilt its
        // roster would find no guests and publish to nobody, forever, while
        // every seat still reads as streaming.
        for guest in &mut roster.guests {
            guest.mark_skipping();
        }
        let ticker = watcher(&shared, |shared| shared.seats[0].ring.pop().is_some());
        let exit = encode_loop(
            &shared,
            &arrivals,
            test_config(Codec::H265),
            &mut roster,
            &mut fake_encoder(),
            None,
        );
        assert!(
            ticker.join().expect("ticker"),
            "the rebuilt loop published to nobody"
        );
        assert_eq!(exit, Exit::Stopped);
        assert_eq!(roster.active.len(), 1, "the seated guest was dropped");
        assert_eq!(roster.guests.len(), 1);
        assert_eq!(roster.controllers.len(), 1);
        drop(held);
    }

    /// The same request against a stream already producing what was asked for
    /// is a refresh, not a rebuild: there is nothing to build differently.
    ///
    /// **The request is made after the guest has settled**, because a guest
    /// that has just joined is owed a keyframe anyway. Asking before that
    /// would let the join keyframe stand in for the answer, and the check
    /// would pass with the request ignored entirely.
    #[test]
    fn a_request_for_what_is_already_running_is_answered_with_a_refresh() {
        let (shared, stream, arrivals) = parked();
        let held = stream.seats().take(wake_handle()).expect("a seat");
        held.declare(lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC);

        let mut roster = Roster::default();
        let forced = Arc::new(AtomicU32::new(0));
        let mut encoder = Fake {
            unit: vec![0x41; 900],
            queued: std::collections::VecDeque::new(),
            forced: Arc::clone(&forced),
            refuse: 0,
            mute: false,
        };

        let counted = Arc::clone(&forced);
        let asked = Arc::clone(&shared);
        let ticker = std::thread::spawn(move || {
            let began = lowlat_common::clock::Time::now();
            let mut settled = None;
            let mut answered = false;
            while lowlat_common::clock::elapsed_ms(began) < 3000.0 {
                // **Keep up on this guest's behalf.** A ring left to fill
                // latches the guest, and a latched guest is owed a keyframe
                // for a reason that has nothing to do with this test: the
                // check would then pass with the request ignored entirely.
                while let Some(index) = asked.seats[0].ring.pop() {
                    drop(asked.pool.claim(index));
                }
                let now = counted.load(Ordering::Relaxed);
                match settled {
                    // The join keyframe, which this test is not about.
                    None if now > 0 => {
                        settled = Some(now);
                        asked.seats[0].reconfigure.store(1, Ordering::Release);
                    }
                    Some(before) if now > before => {
                        answered = true;
                        break;
                    }
                    _ => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            asked.stopping.store(1, Ordering::Release);
            answered
        });

        let exit = encode_loop(
            &shared,
            &arrivals,
            test_config(Codec::H265),
            &mut roster,
            &mut encoder,
            None,
        );
        assert!(
            ticker.join().expect("ticker"),
            "a settled guest asked to reinitialize and got nothing"
        );
        assert_eq!(
            exit,
            Exit::Stopped,
            "an encoder that already fits was rebuilt"
        );
        drop(held);
    }

    /// **A stopped encoder used to be a log line per pass, forever.** The
    /// guests kept their seats and received nothing, and the only thing that
    /// eventually noticed was each peer's own liveness deadline.
    #[test]
    fn an_encoder_that_stops_answering_ends_the_guests_with_a_reason() {
        let (shared, stream, arrivals) = parked();
        let held = stream.seats().take(wake_handle()).expect("a seat");
        held.declare(lowlat_core::init::FLAG_BASE);

        let mut roster = Roster::default();
        let mut encoder = fake_encoder();
        encoder.refuse = u32::MAX;
        // A loop that never notices would hang here rather than fail.
        let done = Arc::new(AtomicU32::new(0));
        let returned = Arc::clone(&done);
        let guarded = Arc::clone(&shared);
        let watchdog = std::thread::spawn(move || {
            let began = lowlat_common::clock::Time::now();
            while returned.load(Ordering::Acquire) == 0
                && lowlat_common::clock::elapsed_ms(began) < 2000.0
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            guarded.stopping.store(1, Ordering::Release);
        });
        let exit = encode_loop(
            &shared,
            &arrivals,
            test_config(Codec::H264),
            &mut roster,
            &mut encoder,
            None,
        );
        done.store(1, Ordering::Release);
        watchdog.join().expect("watchdog");
        shared.stopping.store(0, Ordering::Release);
        assert_eq!(
            exit,
            Exit::Failed(status::ENCODE_FAILED),
            "an encoder that answered nothing was never noticed"
        );
        assert_eq!(
            held.kicked(),
            None,
            "the loop kicked before anyone was told"
        );

        // The reason is left on the seat by the caller of the loop, because
        // only the guest can turn it into a message.
        kick_all(&shared, &roster.active, status::ENCODE_FAILED);
        assert_eq!(held.kicked(), Some(status::ENCODE_FAILED));
    }

    /// **One refusal is not a stopped device.** A device can refuse a collect
    /// and answer the next, and ending every guest over that turns a hiccup
    /// into a disconnection.
    ///
    /// **Written against literals, not against the threshold.** Stating the
    /// count as `COLLECT_FAILURES - 1` makes the check move with the constant,
    /// so lowering the constant to one leaves this refusing nothing at all and
    /// passing while a single hiccup ends every session.
    #[test]
    fn a_single_refusal_is_not_a_stopped_encoder() {
        for refusals in [1u32, 2] {
            let (shared, stream, arrivals) = parked();
            let held = stream.seats().take(wake_handle()).expect("a seat");
            held.declare(lowlat_core::init::FLAG_BASE);

            let mut roster = Roster::default();
            let mut encoder = fake_encoder();
            encoder.refuse = refusals;
            let watchdog = watcher(&shared, |_| false);
            let exit = encode_loop(
                &shared,
                &arrivals,
                test_config(Codec::H264),
                &mut roster,
                &mut encoder,
                None,
            );
            watchdog.join().expect("watchdog");
            assert_eq!(
                exit,
                Exit::Stopped,
                "{refusals} refusal(s) in a row ended the guests"
            );
            drop(held);
        }
    }

    /// A reason reaches every seat, and a seat carries it until its guest
    /// reads it.
    #[test]
    fn a_reason_reaches_every_seat() {
        let (shared, stream, _arrivals) = parked();
        let first = stream.seats().take(wake_handle()).expect("a seat");
        let second = stream.seats().take(wake_handle()).expect("a seat");
        let active = [seat_of(0), seat_of(1)];

        assert_eq!(first.kicked(), None);
        kick_all(&shared, &active, status::NO_ROOM);
        assert_eq!(first.kicked(), Some(status::NO_ROOM));
        assert_eq!(second.kicked(), Some(status::NO_ROOM));
    }

    /// **Only the guests that asked.** A peer rebuilds its decoder the moment
    /// it asks rather than waiting to be told, so a guest whose request failed
    /// holds a decoder for a stream that will never arrive. The guests that
    /// asked for nothing are still watching an encoder that works.
    #[test]
    fn a_failed_request_ends_only_whoever_asked_for_it() {
        let (shared, stream, _arrivals) = parked();
        let asker = stream.seats().take(wake_handle()).expect("a seat");
        let watcher = stream.seats().take(wake_handle()).expect("a seat");
        let active = [seat_of(0), seat_of(1)];

        asker.request_reconfigure();
        assert!(reconfigure_asked(&shared, &active), "the request was lost");
        kick_asked(&shared, &active, status::ENCODER_UNAVAILABLE);

        assert_eq!(asker.kicked(), Some(status::ENCODER_UNAVAILABLE));
        assert_eq!(
            watcher.kicked(),
            None,
            "a guest that asked for nothing lost its picture"
        );
    }

    /// Spin until `predicate` holds or the budget runs out, so a test reports
    /// what it was waiting for rather than hanging.
    fn until(what: &str, predicate: impl FnMut() -> bool) {
        until_within(5000.0, what, predicate);
    }

    /// The same with the budget stated, for runs whose length is the point.
    fn until_within(budget_ms: f64, what: &str, mut predicate: impl FnMut() -> bool) {
        let began = lowlat_common::clock::Time::now();
        while !predicate() {
            assert!(
                lowlat_common::clock::elapsed_ms(began) < budget_ms,
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

    /// The construction path the fake encoder skips entirely: a real device,
    /// a real encoder, and the frames a guest would actually be sent.
    ///
    /// Needs a render node, so it is off by default. Run with
    /// `cargo test -p lowlat-host -- --ignored the_real_encoder`.
    #[test]
    #[ignore = "requires a render node"]
    fn the_real_encoder_serves_a_seated_guest() {
        let stream = Stream::start(Config {
            display: false,
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            backend: Backend::Open,
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
        });
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(wake.handle().expect("handle"))
            .expect("a free seat");

        let mut received = 0usize;
        let mut bytes = 0usize;
        let mut first_is_a_refresh = None;
        until("real frames to arrive", || {
            while let Some(frame) = seat.next_frame() {
                // A coded unit begins with a start code, so this says the
                // bytes are a bitstream and not an empty or partial slot.
                assert_eq!(
                    frame.bytes().get(..4),
                    Some([0, 0, 0, 1].as_slice()),
                    "frame {received} does not begin with a start code"
                );
                if first_is_a_refresh.is_none() {
                    // A sequence parameter set, which only a refresh carries.
                    first_is_a_refresh = frame.bytes().get(4).map(|byte| byte & 0x1F == 7);
                }
                bytes += frame.bytes().len();
                received += 1;
            }
            received >= 60
        });

        // **The first frame a joining guest sees has to be a refresh.** It has
        // no reference chain yet, so anything else decodes to nothing.
        assert_eq!(
            first_is_a_refresh,
            Some(true),
            "the first frame a joining guest received was not a refresh"
        );
        println!("received {received} frames, {bytes} bytes");
    }

    /// Write what a seated guest actually receives, so the exact bytes can be
    /// put through an independent decoder.
    ///
    /// Needs a render node, so it is off by default.
    #[test]
    #[ignore = "requires a render node"]
    fn dump_what_a_guest_receives() {
        let rows: u32 = std::env::var("LOWLAT_DETAIL_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let stream = Stream::start(Config {
            display: false,
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            backend: Backend::Open,
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: rows,
        });
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(wake.handle().expect("handle"))
            .expect("a free seat");

        let mut out = Vec::new();
        let mut sizes = Vec::new();
        until_within(30_000.0, "frames", || {
            while let Some(frame) = seat.next_frame() {
                sizes.push((frame.bytes().len(), frame.keyframe()));
                out.extend_from_slice(frame.bytes());
            }
            sizes.len() >= 400
        });
        std::fs::write("/tmp/lowlat-seat.h264", &out).expect("write");
        // One access-unit length per line, so a consumer can feed a decoder
        // the way a guest's loop does rather than as one stream.
        let index: String = sizes.iter().map(|(len, _)| format!("{len}\n")).collect();
        std::fs::write("/tmp/lowlat-seat.idx", index).expect("write index");
        let body = 1193usize;
        let fragments = |len: usize| (len + 10 + 4).div_ceil(body);
        let multi = sizes.iter().filter(|(len, _)| fragments(*len) > 1).count();
        let largest = sizes.iter().map(|(len, _)| *len).max().unwrap_or(0);
        println!(
            "detail_rows={rows}: {} units, {} bytes, largest {largest} ({} fragments), \
             {multi} need more than one fragment",
            sizes.len(),
            out.len(),
            fragments(largest)
        );
        for (at, (len, key)) in sizes.iter().take(6).enumerate() {
            println!("  unit {at}: {len} bytes, keyframe {key}");
        }
    }

    /// Run the real pipeline at `fps` until it has reported, and print the
    /// table docs/05-host.md section 10 asks for.
    fn measure(fps: u32, frames: usize) -> Report {
        let stream = Stream::start(Config {
            display: false,
            width: 1920,
            height: 1080,
            fps,
            codec: Codec::H264,
            backend: Backend::Open,
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
        });
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(wake.handle().expect("handle"))
            .expect("a free seat");

        let mut received = 0usize;
        // Long enough for the report to be due at the paced rate, which is
        // ten seconds of frames plus the margin a first refresh costs.
        let budget = 4.0 * 1000.0 * frames as f64 / f64::from(fps.max(1)) + 5000.0;
        until_within(budget, "the run to finish", || {
            while seat.next_frame().is_some() {
                received += 1;
            }
            // The report is published on a frame count, so a run shorter than
            // that reads zeros.
            received >= frames && stream.timings().encode.p50 > 0.0
        });

        let report = stream.timings();
        println!("  fps target {fps}, {received} frames");
        for (name, stage) in [
            ("acquire ", report.acquire),
            ("encode  ", report.encode),
            ("publish ", report.publish),
            ("interval", report.interval),
        ] {
            println!(
                "    {name}  p50 {:7.3} ms  p95 {:7.3} ms  p99 {:7.3} ms",
                stage.p50, stage.p95, stage.p99
            );
        }
        println!(
            "    host stages p50 {:.3} ms, p99 {:.3} ms",
            report.host_p50(),
            report.host_p99()
        );
        report
    }

    /// **Gate A item 7.** Every stage reports its percentiles, and the
    /// host-side stages sum to less than one frame interval at the negotiated
    /// rate. A pipeline that cannot clear a frame within a frame interval
    /// cannot hold the frame rate, so this is the floor rather than a target.
    ///
    /// Needs a render node, so it is off by default.
    #[test]
    #[ignore = "requires a render node"]
    fn the_host_stages_clear_a_frame_within_a_frame_interval() {
        let report = measure(60, REPORT_FRAMES as usize + 60);
        let interval = 1000.0 / 60.0;

        assert!(
            report.acquire.p50 > 0.0,
            "the acquire stage reported nothing"
        );
        assert!(report.encode.p50 > 0.0, "the encode stage reported nothing");
        assert!(
            report.host_p50() < interval,
            "the host stages take {:.3} ms at the median, past a {interval:.3} ms frame",
            report.host_p50()
        );
        assert!(
            report.host_p99() < interval,
            "the host stages take {:.3} ms at p99, past a {interval:.3} ms frame",
            report.host_p99()
        );
    }

    /// **Gate A item 6.** The per-stage times sum to more than the wall-clock
    /// interval between frames, and stages that sum past the interval they fit
    /// inside can only have run concurrently.
    ///
    /// **Measured unpaced.** At sixty frames a second on this hardware the
    /// pipeline idles most of the interval, so the sum is under it whether the
    /// stages overlap or not and the arithmetic proves nothing. Removing the
    /// frame clock makes the interval the pipeline's own throughput, which is
    /// where the question has an answer: a serialised loop's interval is the
    /// sum of its stages, and an overlapped one's is the longest of them.
    ///
    /// Needs a render node, so it is off by default.
    #[test]
    #[ignore = "requires a render node"]
    fn encode_overlaps_the_next_frames_preparation() {
        let report = measure(100_000, REPORT_FRAMES as usize + 60);
        let stages = report.acquire.p50 + report.encode.p50 + report.publish.p50;

        assert!(
            stages > report.interval.p50,
            "stages sum to {stages:.3} ms inside a {:.3} ms interval, which is what a \
             serialised pipeline looks like",
            report.interval.p50
        );
        println!(
            "    stages {stages:.3} ms across a {:.3} ms interval, overlap {:.3} ms",
            report.interval.p50,
            stages - report.interval.p50
        );
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
