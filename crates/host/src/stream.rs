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
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;

use lowlat_capture::synthetic::{Marker, Synthetic};
use lowlat_common::spsc::Ring;
use lowlat_encode::{Encoder, Poll};
use lowlat_net::WakeHandle;

use crate::display::Display;
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
/// Audio packets a guest may be behind before one is dropped.
///
/// **Deeper than the picture ring and still short.** A packet is 20 ms, so
/// this is a tenth of a second of slack for a guest whose thread was busy; past
/// that, a late packet is worth less than the one behind it and is dropped
/// rather than queued.
const AUDIO_DEPTH: usize = 5;

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

/// Whether this picture has to be sent even if it is the previous one.
///
/// **Every reason is enumerated here and nowhere else**, because the failure
/// this guards against is a screen that stops updating, and that is invisible
/// in every test that does not look for it. `since_forced_ms` is the time since
/// a picture was last actually submitted, which is what the heartbeat bounds.
fn must_send(changed: bool, refresh: bool, seats_moved: bool, since_forced_ms: f64) -> bool {
    changed || refresh || seats_moved || since_forced_ms >= HEARTBEAT_MS
}

/// How long a picture may be suppressed before one is sent anyway.
///
/// **Not a cadence, a bound on being wrong.** The summary that decides a
/// picture is unchanged is sixty-four bits, so two different pictures can in
/// principle agree; more to the point, every other reason a frame might be
/// owed is enumerated by hand, and this is what limits the damage when that
/// enumeration turns out to be incomplete. A frozen screen for a second is
/// recoverable and a frozen screen forever is not.
const HEARTBEAT_MS: f64 = 1000.0;

/// How often the device is asked whether anything is still plugged into it.
///
/// **Slow on purpose.** It walks every connector on the card, and the state it
/// is looking for is a person moving a cable.
const ATTACHED_MS: f64 = 1000.0;

/// How often the pointer is looked at.
///
/// **An absolute figure and not a frame count.** A pointer that blinks for a
/// single frame must not flap a peer between states, and nothing a hand does
/// to a mouse is worth reporting faster than this. It is also what keeps the
/// read off the frame path: the plane is mapped and compared on the processor,
/// which is cheap at this rate and waste at every frame.
const POINTER_MS: f64 = 18.0;

/// Frames between one timing report and the next. Ten seconds at sixty.
const REPORT_FRAMES: u32 = 600;

/// What a seat is doing. See the module note; each transition has one owner.
mod seat_state {
    pub(super) const FREE: u32 = 0;
    pub(super) const CLAIMED: u32 = 1;
    pub(super) const STREAMING: u32 = 2;
    pub(super) const LEAVING: u32 = 3;
}

/// What can be changed about sound while a host runs.
///
/// **Atomics rather than a lock**, because two of these are read on the path
/// that publishes every packet and the third on the path that prices it.
#[derive(Debug)]
struct SoundCells {
    /// Whether sound is captured at all.
    on: AtomicU32,
    /// Whether a guest that asked for the uncompressed form may have it.
    allow_raw: AtomicU32,
    /// What the compressed form is encoded at.
    kbps: AtomicU32,
    /// Whether a guest's microphone is taken.
    accept_mic: AtomicU32,
}

impl SoundCells {
    fn new(config: &Config) -> Self {
        Self {
            // **Both, and not either.** A host with no source cannot be
            // switched on, and one that has a source can arrive switched off.
            on: AtomicU32::new(u32::from(config.audio_on && config.audio.is_some())),
            allow_raw: AtomicU32::new(u32::from(config.allow_raw_audio)),
            kbps: AtomicU32::new(config.audio_kbps),
            accept_mic: AtomicU32::new(u32::from(config.accept_microphone)),
        }
    }
}

/// What sound is set to, in both directions.
///
/// **One structure rather than a handful of arguments**, because every one of
/// these is live and they are read and written together; a call that took them
/// positionally would be four booleans at the call site inside a month.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SoundSettings {
    pub on: bool,
    pub allow_raw: bool,
    pub kbps: u32,
    pub accept_microphone: bool,
    pub live: lowlat_audio::Live,
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
    /// Audio packets, loop to guest, in the same shape as the picture ring
    /// beside it: single producer, single consumer, indices into the audio
    /// pool.
    audio: Ring<u32, AUDIO_DEPTH>,
    /// Whether this guest asked for the uncompressed form, from its own
    /// initialization. Read by the thread that captures, so it knows which
    /// encodings the room actually wants.
    wants_raw: AtomicU32,
    /// How to wake this guest when sound is published to it.
    ///
    /// **A second handle, because sound is published by a different thread**
    /// from the one that publishes pictures, and neither may hold the other's.
    /// Locked once per packet and never contended: only this seat's own guest
    /// writes it, and only when taking or leaving the seat.
    audio_wake: std::sync::Mutex<Option<WakeHandle>>,
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
            audio: Ring::new(),
            wants_raw: AtomicU32::new(0),
            audio_wake: std::sync::Mutex::new(None),
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
pub(crate) struct Shared {
    seats: [Seat; MAX_SEATS],
    pool: Pool,
    /// Sound, in its own pool because it is produced by its own thread on its
    /// own cadence. One slot per encoding a room actually wants, never one per
    /// guest.
    audio: Pool,
    /// The sound device, held exactly while somebody is listening.
    ///
    /// **Here rather than on the loop's stack**, because the loop that knows
    /// the room is empty is not the one that built it: `run` waits for a guest
    /// and `encode_loop` runs while there are none, so a device owned by the
    /// outer one is never given back. That is not hypothetical -- it held a
    /// capture and somebody's muted speakers across three sessions.
    held_sound: std::sync::Mutex<Option<crate::audio::Sound>>,
    /// Whether the room wants sound now, and a counter of how often that
    /// answer has changed.
    ///
    /// **The decision is recorded even when no device is configured**, which is
    /// what makes it testable without one.
    sound_demand: AtomicU32,
    sound_epoch: AtomicU32,
    /// The half of sound's settings the capture thread reads for itself.
    ///
    /// **Held here rather than by the capture**, so what an application asked
    /// for survives the device being closed and reopened when the room empties
    /// and fills again.
    sound_live: Arc<lowlat_audio::Wanted>,
    /// What sound is set to now.
    ///
    /// **Read on the frame that uses it rather than latched**, because every
    /// one of these is cheap to consult and a stale copy would be a host
    /// running settings nobody can see.
    sound: SoundCells,
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
    /// The last refresh window, published beside the timings.
    refreshes: RefreshCells,
    /// Pictures the loop decided nobody needed, since the stream started.
    ///
    /// **Cumulative rather than per window**, because what it answers is
    /// whether suppression is working at all, and a reader that samples it
    /// twice gets the rate for free.
    suppressed: AtomicU32,
    /// What is being captured, as a checksum of its name. Zero until a display
    /// has been opened.
    ///
    /// **A checksum because a name is a string and this must stay lock free**,
    /// and a reader has the names already: it matches this against the outputs
    /// it can enumerate. A miss is not a hazard, only a reader that has to fall
    /// back to what the host *would* choose.
    ///
    /// **Published rather than inferred, because the loop can change it on its
    /// own.** A display that moves to another card is followed here, and
    /// nothing above would know.
    captured: AtomicU32,
    /// Bumped when a different output has been asked for.
    ///
    /// **A counter rather than the name.** The name is a string and cannot be
    /// an atomic, but the loop does not need it: it needs only to know that it
    /// must hand the encoder back, and the pass that rebuilds reads the name
    /// from the channel where it can own it.
    output_asked: AtomicU32,
    /// Bumped when a live video setting changes.
    ///
    /// **A counter and a lock, rather than packed atomics.** The loop reads the
    /// counter every pass and takes the lock only when it moved, so the frame
    /// path pays one load and the lock is held once per change rather than once
    /// per frame (AGENTS 6.8).
    video_asked: AtomicU32,
    video: std::sync::Mutex<LiveVideo>,
    /// Where this loop announces what only it knows.
    ///
    /// **Here rather than threaded through every call.** Everything else the
    /// loop publishes goes through this structure already, and what is being
    /// captured is published from the one place both backends meet.
    raise: Option<crate::events::Sender>,
    /// Raised to end the loop; the loop checks it once per frame.
    stopping: AtomicU32,
    /// The picture the stream is really producing, as width in the high half
    /// and height in the low.
    ///
    /// **Not the configured size.** A display decides its own, and a guest
    /// that describes the stream with the configured numbers tells its peer a
    /// coordinate space the picture is not in: the peer then sends absolute
    /// input against a rectangle of the wrong size, and every position lands
    /// scaled by the ratio between the two.
    picture: AtomicU32,
    /// The captured output's rectangle in the desktop, four sixteen-bit fields
    /// from the top: x, y, width, height. Zero until a session has described
    /// it, and zero is also what one output honestly is.
    ///
    /// **The size here is the desktop's units and `picture` is the picture's**,
    /// which differ by the display scale. Keeping both is what lets one place
    /// convert between them instead of every reader assuming they agree.
    place_rect: AtomicU64,
    /// The desktop that rectangle sits in, width in the high half.
    ///
    /// **Stored before the rectangle and read after it**, so a reader that
    /// sees a rectangle sees the desktop it was measured against. Written once
    /// per display, and a reader that manages to catch the pair mid-write
    /// takes the whole of it again on its next pass.
    place_desktop: AtomicU32,
    /// Where a guest last told the pointer to be, and how many times one has:
    /// the position in the low half and a count in the high.
    ///
    /// **The only thing on this host that knows where the pointer was asked to
    /// go.** Nothing reports a pointer's hotspot, and the difference between
    /// this and where the display then drew the pointer is exactly it. The
    /// count is what says a command is not the one already accounted for.
    commanded: AtomicU64,
    /// The pointer, as the loop last read it.
    cursor: CursorCell,
    /// Counts encoder reinitializations.
    ///
    /// **A guest has to know, and cannot infer it.** A new encoder is a new
    /// reference chain and a new set of parameter sets, which the peer learns
    /// from the generation in the video header; a guest that never noticed
    /// would keep announcing the old one.
    epoch: AtomicU32,
}

/// The pointer as a guest needs to report it, in the captured picture's own
/// coordinates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PointerState {
    pub x: u16,
    pub y: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// The drawn part's size, which is what travels; a plane is a fixed
    /// allocation and almost all of it is padding.
    pub width: u16,
    pub height: u16,
    /// Names the picture, for a peer that keeps them.
    pub checksum: u32,
    /// Nothing is compositing a pointer, held long enough to mean it.
    ///
    /// **This is the bit a client reads as relative mode** (`relative ||
    /// hidden`), so it takes a guest's pointer away and turns its motion into
    /// deltas. That is wanted when an application took the pointer over, and
    /// this backend cannot see that directly: what it sees is the hardware
    /// plane emptying, which happens for that reason and for two others.
    pub hidden: bool,
}

/// How long the pointer plane must stay empty before it means anything.
///
/// **The plane emptying is a noisy version of "an application took the
/// pointer".** It is that when a game hides the cursor to aim, and it is not
/// when a pointer merely grew past what the plane can carry, or for the moment
/// after a display mode change while the pipeline is being rebuilt. Both of
/// those pass; an application holding the pointer does not.
///
/// So it is slow to hide and immediate to show. A quarter second before a
/// pointer disappears on entering a game is not noticeable; a quarter second
/// of a guest wrongly locked into relative motion is, which is why the other
/// edge has no delay at all.
const HIDDEN_AFTER_MS: f64 = 250.0;

/// Whether the display is drawing a pointer, and since when.
#[derive(Debug, Default)]
struct Presence {
    /// When the plane was first found empty, while it still is.
    dark_since: Option<f64>,
    /// The last pointer actually seen, which is what a report of nothing being
    /// drawn has to carry: the plane holds the position too, so there is no
    /// fresh one to send.
    ///
    /// **Absent until one has been seen, and that is load bearing.** "Nothing
    /// is drawn" only means anything against something having been drawn
    /// before. A stream can start onto an idle desktop whose compositor is not
    /// using the pointer plane at all, and reporting that as a pointer taken
    /// over tells a guest to hide its cursor and switch to deltas before it
    /// has been shown a pointer even once.
    last: Option<PointerState>,
}

/// One read of the pointer, as the presence rule needs it.
#[derive(Debug, Clone, Copy)]
struct Seen {
    state: PointerState,
    /// Whether this read examined the pixels. See [`crate::capture`].
    looked: bool,
}

impl Presence {
    /// Take one read into account and say what to publish, if anything.
    ///
    /// **The whole rule in one place, because it was got wrong at the call
    /// site rather than here.** A read that did not look at the pixels cannot
    /// say a pointer is still there, and letting one clear the wait resets it
    /// three times out of four, so it never expires.
    fn observe(&mut self, seen: Option<Seen>, now_ms: f64) -> Option<PointerState> {
        let Some(seen) = seen else {
            return self.dark(now_ms);
        };
        if seen.looked {
            self.lit(seen.state);
            return Some(seen.state);
        }
        // Mid-decision: saying anything here reports a pointer that may
        // already be gone, and undoes the wait that is about to answer.
        if self.waiting() {
            return None;
        }
        Some(PointerState {
            x: seen.state.x,
            y: seen.state.y,
            ..self.carried()
        })
    }

    /// Nothing is being drawn. Say so, once it has been true long enough.
    fn dark(&mut self, now_ms: f64) -> Option<PointerState> {
        // Nothing has ever been drawn, so nothing has stopped being drawn.
        let last = self.last?;
        let since = *self.dark_since.get_or_insert(now_ms);
        if now_ms - since < HIDDEN_AFTER_MS {
            return None;
        }
        let state = PointerState {
            hidden: true,
            ..last
        };
        self.last = Some(state);
        Some(state)
    }

    /// Whether a decision about the pointer going away is pending.
    ///
    /// While it is, a read that did not look at the pixels says nothing: it
    /// would report a pointer that may already be gone.
    fn waiting(&self) -> bool {
        self.dark_since.is_some()
    }

    /// The last pointer seen, for a read that only has a position to add.
    fn carried(&self) -> PointerState {
        self.last.unwrap_or_default()
    }

    /// A pointer is being drawn again.
    ///
    /// **Immediate, and deliberately not debounced**: the cost of being late
    /// on this edge is a guest that cannot see its own pointer for that long.
    fn lit(&mut self, state: PointerState) {
        self.dark_since = None;
        self.last = Some(state);
    }
}

/// The pointer the loop last saw, and a counter that says so cheaply.
///
/// **The one lock this stream has, and the exception the rule allows** (AGENTS
/// section 6 rule 8): many readers during a stream, one writer, and a
/// variable-length picture that cannot be an atomic. A guest copies under it
/// and does everything else outside it, and the counter beside it is what
/// keeps a guest from taking it at all on the passes where nothing moved,
/// which is almost all of them.
#[derive(Debug, Default)]
struct CursorCell {
    generation: AtomicU32,
    held: std::sync::Mutex<Held>,
}

/// What a guest copies out.
#[derive(Debug, Default)]
struct Held {
    state: PointerState,
    png: Vec<u8>,
}

impl Shared {
    /// Which encodings the seated guests actually want.
    ///
    /// **Asked once per frame rather than assumed.** A room with nobody in it
    /// wants neither, and producing either would be work nobody receives.
    pub(crate) fn audio_wanted(&self) -> (bool, bool) {
        let mut compressed = false;
        let mut uncompressed = false;
        for index in 0..self.seats.len() {
            match self.seat_raw(index) {
                None => {}
                Some(false) => compressed = true,
                Some(true) => uncompressed = true,
            }
        }
        (compressed, uncompressed)
    }

    /// What one seat is actually sent, or `None` when it is not listening.
    ///
    /// **A guest asks and a host permits**, and this is where the two meet.
    /// Everything that produces, prices or labels a packet reads it here, so a
    /// permission withdrawn while a guest is connected reaches all three at
    /// once rather than in whatever order they happen to be called.
    pub(crate) fn seat_raw(&self, index: usize) -> Option<bool> {
        let seat = self.seats.get(index)?;
        if seat.state.load(Ordering::Acquire) != seat_state::STREAMING {
            return None;
        }
        let asked = seat.wants_raw.load(Ordering::Relaxed) != 0;
        Some(asked && self.sound.allow_raw.load(Ordering::Relaxed) != 0)
    }

    /// The settings the capture thread reads for itself.
    pub(crate) fn sound_wanted(&self) -> &Arc<lowlat_audio::Wanted> {
        &self.sound_live
    }

    /// Whether sound is being captured at all.
    pub(crate) fn sound_on(&self) -> bool {
        self.sound.on.load(Ordering::Relaxed) != 0
    }

    /// Whether a guest that asked for the uncompressed form may have it.
    pub(crate) fn sound_allow_raw(&self) -> bool {
        self.sound.allow_raw.load(Ordering::Relaxed) != 0
    }

    /// What the compressed form is encoded at.
    pub(crate) fn sound_kbps(&self) -> u32 {
        self.sound.kbps.load(Ordering::Relaxed)
    }

    /// Whether a guest's microphone is taken.
    pub(crate) fn accept_microphone(&self) -> bool {
        self.sound.accept_mic.load(Ordering::Relaxed) != 0
    }

    /// Change what sound is set to, while it runs.
    pub(crate) fn set_sound(&self, on: bool, allow_raw: bool, kbps: u32) {
        self.sound.on.store(u32::from(on), Ordering::Relaxed);
        self.sound
            .allow_raw
            .store(u32::from(allow_raw), Ordering::Relaxed);
        self.sound.kbps.store(kbps.max(1), Ordering::Relaxed);
    }

    /// The same for the uplink, which is a separate decision.
    pub(crate) fn set_accept_microphone(&self, accept: bool) {
        self.sound
            .accept_mic
            .store(u32::from(accept), Ordering::Relaxed);
    }

    /// What the room's sound costs, across every guest listening to it.
    ///
    /// **Walked rather than counted as guests arrive**, because a guest's
    /// choice lands after it is seated: it declares the encoding it wants when
    /// its initialization is parsed, which is a pass or two later.
    pub(crate) fn audio_mbps(&self, compressed_kbps: u32) -> f64 {
        let mut total = 0.0;
        for index in 0..self.seats.len() {
            if let Some(raw) = self.seat_raw(index) {
                total += crate::audio::guest_mbps(raw, compressed_kbps);
            }
        }
        total
    }

    /// Hand one packet to every seat that asked for this encoding.
    ///
    /// **A slot per encoding, not per guest**: the pool holds the bytes once
    /// and every guest is given the index. A seat whose ring is full keeps its
    /// place and loses this packet, which is what a late packet is worth.
    pub(crate) fn publish_audio(&self, raw: bool, payload: &[u8]) {
        let Some(mut writer) = self.audio.acquire() else {
            // Every slot is still held, which means every guest is behind on
            // sound. Dropping is the whole policy: nothing here is
            // retransmitted and nothing depends on the packet before it.
            return;
        };
        if !writer.fill(payload) {
            return;
        }
        let Some(filler) = self.seats.first() else {
            return;
        };
        let mut rings: [&Ring<u32, AUDIO_DEPTH>; MAX_SEATS] = [&filler.audio; MAX_SEATS];
        let mut waking: [usize; MAX_SEATS] = [0; MAX_SEATS];
        let mut n = 0usize;
        for (index, seat) in self.seats.iter().enumerate() {
            if self.seat_raw(index) != Some(raw) {
                continue;
            }
            if let (Some(target), Some(who)) = (rings.get_mut(n), waking.get_mut(n)) {
                *target = &seat.audio;
                *who = index;
                n += 1;
            }
        }
        // The keyframe flag means nothing to sound; the pool carries one for
        // pictures and this path leaves it clear.
        let _ = writer.publish(false, rings.get(..n).unwrap_or(&[]));
        for index in waking.get(..n).unwrap_or(&[]) {
            if let Some(seat) = self.seats.get(*index)
                && let Ok(held) = seat.audio_wake.lock()
                && let Some(wake) = held.as_ref()
            {
                let _ = wake.notify();
            }
        }
    }

    /// Where a guest just told the pointer to be, and the count that goes with
    /// it.
    fn commanded(&self) -> Option<(u64, u16, u16)> {
        let packed = self.commanded.load(Ordering::Acquire);
        let count = packed >> 32;
        if count == 0 {
            return None;
        }
        let x = u16::try_from((packed >> 16) & 0xFFFF).ok()?;
        let y = u16::try_from(packed & 0xFFFF).ok()?;
        Some((count, x, y))
    }

    /// Say what size the stream is really producing.
    fn publish_picture(&self, width: u32, height: u32) {
        let packed = (width.min(0xFFFF) << 16) | height.min(0xFFFF);
        self.picture.store(packed, Ordering::Release);
    }

    /// Say which output is being captured.
    fn publish_captured(&self, id: Option<&str>) {
        let checksum = id.map_or(0, |id| lowlat_core::crc32::of(id.as_bytes()));
        self.captured.store(checksum, Ordering::Release);
    }

    /// Say where the captured output sits in the desktop around it.
    ///
    /// Nothing said is one output, which needs no placing: the absolute axis
    /// already spans exactly the picture.
    fn publish_place(&self, place: Option<lowlat_capture::desktop::Placement>) {
        let Some(place) = place else {
            self.place_rect.store(0, Ordering::Release);
            return;
        };
        let field = |value: u32| u64::from(value.min(0xFFFF));
        self.place_desktop.store(
            (place.desktop_width.min(0xFFFF) << 16) | place.desktop_height.min(0xFFFF),
            Ordering::Release,
        );
        let packed = (field(place.x) << 48)
            | (field(place.y) << 32)
            | (field(place.width) << 16)
            | field(place.height);
        self.place_rect.store(packed, Ordering::Release);
    }

    /// Publish what the loop just read of the pointer.
    ///
    /// `image` is the picture, and is present only when it is one no guest has
    /// been shown yet. **Nothing is published when nothing changed**, so a
    /// stationary pointer costs guests one atomic load a pass and no message.
    fn publish_pointer(&self, state: PointerState, image: Option<&[u8]>) {
        let mut held = self
            .cursor
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if held.state == state && image.is_none() {
            return;
        }
        held.state = state;
        if let Some(image) = image {
            held.png.clear();
            held.png.extend_from_slice(image);
        }
        drop(held);
        // Released after the write above, so a guest that sees the new count
        // sees the state that goes with it.
        self.cursor.generation.fetch_add(1, Ordering::Release);
    }
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

/// The last refresh window, for a reader outside the loop.
///
/// **The same arrangement as [`TimingCells`], for the same reason.** The loop
/// owns the counts and publishes a snapshot once a window; a reader never
/// touches the frame path.
#[derive(Debug, Default)]
struct RefreshCells {
    cells: [AtomicU32; 7],
}

impl RefreshCells {
    fn publish(&self, of: &Refreshes) {
        for (cell, value) in self.cells.iter().zip(of.counts()) {
            cell.store(value, Ordering::Relaxed);
        }
    }

    #[cfg(all(test, not(loom)))]
    fn read(&self) -> Refreshes {
        let mut counts = [0u32; 7];
        for (slot, cell) in counts.iter_mut().zip(&self.cells) {
            *slot = cell.load(Ordering::Relaxed);
        }
        Refreshes::from_counts(counts)
    }
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
///
/// **Not `Copy` any more**: the output is named rather than numbered, and a
/// name is as long as the system chose to make it.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where sound is read from, or **nothing for a host with no sound source
    /// at all**, which is a decision taken when the stream is built and never
    /// after.
    ///
    /// A failure to open it is not a failure to host: sound goes off, the
    /// reason is logged once, and the session runs.
    pub audio: Option<lowlat_audio::Config>,
    /// Whether a guest's microphone is taken.
    ///
    /// **Off by default and its own decision.** Taking one means accepting a
    /// packet every ten milliseconds on the channel the control messages share
    /// -- and telling the peer we will, without which it sends nothing at all.
    pub accept_microphone: bool,
    /// Whether sound is switched on.
    ///
    /// **Separate from having a source, because this one is live.** Switching
    /// sound off gives the device back and switching it on takes it again, and
    /// a host that arrived here switched off must still be able to do the
    /// second -- so what a caller can change and what it cannot are two fields
    /// rather than the presence of one.
    pub audio_on: bool,
    /// What the compressed form is encoded at.
    pub audio_kbps: u32,
    /// **A permission, not a request.** A guest asks for the uncompressed form
    /// in its own initialization; this is whether a host will serve it, and it
    /// costs an order of magnitude more of the uplink.
    pub allow_raw_audio: bool,
    pub codec: Codec,
    /// Which encoder to build, or **nothing to follow the display**.
    ///
    /// **Following is the right default and choosing is the override.** A
    /// conversion target is allocated on the device the display is on, and an
    /// encoder belonging to another device cannot take it: on a machine with
    /// two cards, a backend picked in advance is right only while the display
    /// stays where it was. Configuring one is for forcing a particular
    /// encoder on a machine where either would do.
    pub backend: Option<Backend>,
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
    /// Which output to capture, by the name [`crate::display::Display::outputs`]
    /// gives, or the first one lit.
    ///
    /// **Not a fallback when it names nothing.** A host asked for one screen
    /// and given another looks like it worked, and the person who asked is
    /// looking at the wrong desk.
    pub output: Option<String>,
    /// Capture the real display rather than generating pictures.
    ///
    /// **Not a preference between two sources of equal standing.** The
    /// generator exists so everything above it can be built and measured
    /// without a screen; a display is what the product streams. Which node
    /// that is is not configured, because it is discovered: see
    /// [`crate::display::Display::open`].
    pub display: bool,
    /// Whether to emit at `fps` even when the picture has not changed.
    ///
    /// **The initial value of the live setting**, which is why it is here as
    /// well as there: without it the cell is built from a default and whatever
    /// a caller set when the host started is silently dropped.
    pub full_fps: bool,
    /// Which congestion control level every guest's controller runs at.
    ///
    /// **Level 0 is the most aggressive, not "off"** ([`lowlat_core::congestion`]),
    /// and it is compatibility-only. This was pinned at 0 in the guest loop and
    /// nothing configured it, so every session ran the setting that declares
    /// congestion on any stale fragment.
    pub cg_level: usize,
}

/// The part of a video configuration that changes while the stream runs.
///
/// **Everything here is applied without rebuilding anything.** The bitrate
/// re-bases the budget and reaches the encoder through the reconfigure it
/// already does every pass; the frame rate changes the pacing. A field that
/// needed the encoder rebuilt would belong with the codec instead, which is
/// settled once and never moves.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LiveVideo {
    /// **A ceiling, not a target.** Capture runs at the display's own rate.
    pub fps: u32,
    pub bitrate_mbps: f64,
    pub min_mbps: f64,
    /// Whether to emit at `fps` even when the picture has not changed.
    ///
    /// **Off by default, which is a statement of intent rather than of
    /// behaviour.** There is no damage signal here at all, so nothing yet
    /// skips a repeated picture; what this clears is the *permission* to send
    /// one anyway, and a host that keeps sending costs bitrate rather than
    /// being wrong. Defaulting it on would promise to spend that bitrate
    /// forever, which is not what anybody wants asked for on their behalf.
    pub full_fps: bool,
}

impl Default for LiveVideo {
    fn default() -> Self {
        Self {
            fps: 60,
            bitrate_mbps: 10.0,
            min_mbps: 1.0,
            full_fps: false,
        }
    }
}

/// The running loop, and the handle the seam holds it by.
#[derive(Debug)]
pub struct Stream {
    shared: Arc<Shared>,
    joins: Option<mpsc::Sender<Join>>,
    /// Where a request to capture something else is put.
    ///
    /// **Its own channel rather than a lock.** The loop reads it once per
    /// rebuild and never on a frame, and a channel keeps the name owned by
    /// one side at a time without the stream growing a second mutex.
    outputs: Option<mpsc::Sender<Option<String>>>,
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
        Self::announcing(config, None)
    }

    /// The same, raising what only this loop can know: what it is capturing
    /// now, and that it cannot continue.
    pub fn announcing(config: Config, raise: Option<crate::events::Sender>) -> Self {
        let shared = Arc::new(Shared {
            seats: core::array::from_fn(|_| Seat::new()),
            pool: Pool::new(POOL_SLOTS, max_frame_bytes()),
            audio: crate::audio::pool(),
            held_sound: std::sync::Mutex::new(None),
            sound_demand: AtomicU32::new(0),
            sound_epoch: AtomicU32::new(0),
            sound_live: config.audio.as_ref().map_or_else(
                || Arc::new(lowlat_audio::Wanted::default()),
                |audio| Arc::clone(&audio.wanted),
            ),
            sound: SoundCells::new(&config),
            encode_us: AtomicU32::new(0),
            timing: TimingCells::default(),
            refreshes: RefreshCells::default(),
            suppressed: AtomicU32::new(0),
            picture: AtomicU32::new(0),
            place_rect: AtomicU64::new(0),
            place_desktop: AtomicU32::new(0),
            commanded: AtomicU64::new(0),
            cursor: CursorCell::default(),
            stopping: AtomicU32::new(0),
            captured: AtomicU32::new(0),
            output_asked: AtomicU32::new(0),
            raise: raise.clone(),
            video_asked: AtomicU32::new(0),
            video: std::sync::Mutex::new(LiveVideo {
                fps: config.fps,
                bitrate_mbps: config.configured_mbps,
                min_mbps: config.min_mbps,
                full_fps: config.full_fps,
            }),
            epoch: AtomicU32::new(0),
        });
        let (joins, arrivals) = mpsc::channel();
        let (outputs, asked) = mpsc::channel();
        let owned = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("lowlat-stream".to_string())
            .spawn(move || run(&owned, &arrivals, &asked, config))
            .ok();
        Self {
            shared,
            joins: Some(joins),
            outputs: Some(outputs),
            thread,
        }
    }

    /// Change what sound is set to, while a host runs.
    ///
    /// **Every one of these is live.** Turning sound off gives the device back
    /// and restores the speakers, exactly as the last guest leaving does; the
    /// rest are read on the frame that uses them.
    pub fn set_audio(&self, settings: &SoundSettings) {
        self.shared
            .set_sound(settings.on, settings.allow_raw, settings.kbps);
        self.shared
            .set_accept_microphone(settings.accept_microphone);
        self.shared.sound_wanted().set(&settings.live);
    }

    /// What sound is set to now.
    pub fn audio(&self) -> SoundSettings {
        SoundSettings {
            on: self.shared.sound_on(),
            allow_raw: self.shared.sound_allow_raw(),
            kbps: self.shared.sound_kbps(),
            accept_microphone: self.shared.accept_microphone(),
            live: self.shared.sound_wanted().read(),
        }
    }

    /// What sound is **doing** now: whether a device is being read, and which.
    ///
    /// Different from the settings above in the way the picture's size is
    /// different from the configured one. Sound is off in an empty room
    /// however it is configured, a capture that could not be opened is not
    /// reading anything, and an empty device asks for the default rather than
    /// naming what that turned out to be.
    ///
    /// **Tried rather than waited for.** The loop holds this while it opens a
    /// device, which can take seconds against a sound server that is not
    /// answering, and a caller asking what is happening must not be parked
    /// behind that. Busy means the loop is opening or closing it, which is not
    /// reading either.
    pub fn sound_state(&self) -> (bool, Option<String>) {
        self.shared
            .held_sound
            .try_lock()
            .map_or((false, None), |held| {
                (
                    held.as_ref().is_some_and(crate::audio::Sound::alive),
                    held.as_ref().and_then(crate::audio::Sound::device),
                )
            })
    }

    /// Capture a different output, without ending anybody's session.
    ///
    /// **It costs one coded refresh and nothing else.** The encoder and the
    /// conversion are built around one picture, so a different source is the
    /// same rebuild a display changing size already is: the guests keep their
    /// seats and their channel, and are told the reference chain restarted.
    /// Nothing is special-cased for two outputs of equal size, because the
    /// content is entirely different and the refresh is owed either way.
    pub fn select_output(&self, id: Option<String>) {
        let Some(outputs) = self.outputs.as_ref() else {
            return;
        };
        // **Named first, then announced.** The loop reads the counter and only
        // then takes the name, so a name that had not arrived yet would make
        // it rebuild onto the one it already had.
        if outputs.send(id).is_ok() {
            self.shared.output_asked.fetch_add(1, Ordering::Release);
        }
    }

    /// Change what can be changed without rebuilding anything.
    ///
    /// **Named before it is announced**, exactly as an output change is: the
    /// loop reads the counter and only then takes the values, so a value that
    /// had not landed yet would be applied a pass late.
    pub fn set_video(&self, video: LiveVideo) {
        let Ok(mut held) = self.shared.video.lock() else {
            return;
        };
        *held = video;
        drop(held);
        self.shared.video_asked.fetch_add(1, Ordering::Release);
    }

    /// What the stream is running at now.
    pub fn video(&self) -> LiveVideo {
        self.shared
            .video
            .lock()
            .map_or_else(|held| *held.into_inner(), |held| *held)
    }

    /// What size the stream is really producing, once it is known.
    ///
    /// **The picture, never the configuration.** A display decides its own
    /// size and the stream follows it, so anything describing this stream from
    /// what it was configured with describes a stream nobody is producing
    /// (docs/05-host.md section 7).
    pub fn picture(&self) -> Option<(u32, u32)> {
        let packed = self.shared.picture.load(Ordering::Acquire);
        if packed == 0 {
            return None;
        }
        Some((packed >> 16, packed & 0xFFFF))
    }

    /// What is being captured, as a checksum of its name.
    ///
    /// Zero before a display has been opened. See [`Shared::captured`].
    pub fn captured(&self) -> u32 {
        self.shared.captured.load(Ordering::Acquire)
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
    pub fn take(&self, wake: WakeHandle, audio_wake: WakeHandle) -> Option<SeatHold> {
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
                // **Cleared with the rest of the declaration**, or an arriving
                // guest is sent whatever encoding the last occupant asked for
                // until its own initialization lands.
                seat.wants_raw.store(0, Ordering::Relaxed);
                if let Ok(mut held) = seat.audio_wake.lock() {
                    *held = Some(audio_wake);
                }
                if joins.send(Join { seat: index, wake }).is_err() {
                    if let Ok(mut held) = seat.audio_wake.lock() {
                        *held = None;
                    }
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

    /// The next published packet of sound, or `None`.
    ///
    /// Releases its hold on the pool slot when it is dropped, exactly as a
    /// picture does.
    pub fn next_audio(&self) -> Option<frames::Frame<'_>> {
        let seat = self.shared.seats.get(self.index)?;
        let index = seat.audio.pop()?;
        self.shared.audio.claim(index)
    }

    /// What this guest is actually sent: what it asked for, as far as the host
    /// permits.
    ///
    /// **The header must say what the payload is**, so the guest's own loop
    /// reads this rather than remembering what the peer asked for.
    pub fn audio_raw(&self) -> bool {
        self.shared.seat_raw(self.index).unwrap_or(false)
    }

    /// Whether this host is taking a guest's microphone.
    ///
    /// **Read every pass rather than latched**, because it is live: a host
    /// that stops taking one has to tell the peer, and a peer that was never
    /// told keeps its microphone muted.
    pub fn accept_microphone(&self) -> bool {
        self.shared.accept_microphone()
    }

    /// Say which encoding this guest asked for.
    ///
    /// **From its own initialization, and it may say so more than once.** A
    /// peer that repeats its declaration is not changing anything; a peer that
    /// changes it is answered from the next packet.
    pub fn declare_audio(&self, raw: bool) {
        if let Some(seat) = self.shared.seats.get(self.index) {
            seat.wants_raw.store(u32::from(raw), Ordering::Relaxed);
        }
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

    /// What size the stream is really producing, once it is known.
    ///
    /// **Read when a guest takes its seat**, which is after the loop has
    /// settled the size, and it is what the peer has to be told: the picture's
    /// own pixels are the space its absolute input is expressed in.
    pub fn picture(&self) -> Option<(u16, u16)> {
        let packed = self.shared.picture.load(Ordering::Acquire);
        if packed == 0 {
            return None;
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "both halves were clamped to sixteen bits when they were stored"
        )]
        Some(((packed >> 16) as u16, (packed & 0xFFFF) as u16))
    }

    /// Where the captured output sits in the desktop, when a session said.
    ///
    /// **Read every pass beside the picture's size**, and for the same reason:
    /// a guest takes its seat before the loop has opened a display, so the
    /// answer at that moment is that nothing is known yet.
    pub fn place(&self) -> Option<lowlat_inject::event::Place> {
        let rect = self.shared.place_rect.load(Ordering::Acquire);
        if rect == 0 {
            return None;
        }
        let desktop = self.shared.place_desktop.load(Ordering::Acquire);
        let field = |shift: u32| u32::try_from((rect >> shift) & 0xFFFF).unwrap_or(0);
        Some(lowlat_inject::event::Place {
            x: field(48),
            y: field(32),
            width: field(16),
            height: field(0),
            desktop_width: desktop >> 16,
            desktop_height: desktop & 0xFFFF,
        })
    }

    /// Say where this guest just told the pointer to be.
    ///
    /// **Only what was really injected.** A position the permission gate or
    /// the arbiter refused was never commanded, and reading it as though it
    /// were would learn a hotspot from a pointer that never moved.
    pub fn command_pointer(&self, x: u16, y: u16) {
        let previous = self.shared.commanded.load(Ordering::Relaxed);
        // Wrapped inside its own half, and never back to zero, because zero is
        // what "nothing has commanded anything" is spelled as.
        let count = ((previous >> 32).wrapping_add(1)) & 0xFFFF_FFFF;
        let count = if count == 0 { 1 } else { count };
        let packed = (count << 32) | (u64::from(x) << 16) | u64::from(y);
        self.shared.commanded.store(packed, Ordering::Release);
    }

    /// How many times the pointer has changed.
    ///
    /// **Read every pass and the lock is not**, which is the point of it:
    /// nothing about the pointer moves on most passes.
    pub fn pointer_generation(&self) -> u32 {
        self.shared.cursor.generation.load(Ordering::Acquire)
    }

    /// Copy the pointer out, picture included.
    ///
    /// **Copied under the lock and used outside it.** Encoding a message while
    /// holding it would put the whole of a guest's pointer work inside a lock
    /// every other guest is waiting on.
    pub fn pointer(&self, image: &mut Vec<u8>) -> Option<PointerState> {
        let held = self
            .shared
            .cursor
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        image.clear();
        image.extend_from_slice(&held.png);
        Some(held.state)
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
fn run(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    asked: &mpsc::Receiver<Option<String>>,
    mut config: Config,
) {
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
            reconcile_sound(shared, 0, &config);
            if shared.stopping.load(Ordering::Acquire) != 0 {
                return;
            }
            std::thread::sleep(IDLE_WAIT);
        }

        // **Taken before the encoder**, so the first guest hears the desktop
        // while the picture is still being built.
        reconcile_sound(shared, 1, &config);

        // Drained here, where the configuration is owned.
        if let Some(id) = requested(asked) {
            config.output = id;
            lowlat_common::log_info!(
                "stream: capturing {}",
                config
                    .output
                    .as_deref()
                    .unwrap_or("the first output that is lit")
            );
        }

        // **Resolved on every rebuild, not once.** A display can move to
        // another card while this is running, and the rebuild that follows it
        // is the only chance to build an encoder that can take its frames.
        let backend = match config.backend {
            Some(chosen) => chosen,
            None => follow_display(&config),
        };

        lowlat_common::log_info!(
            "stream: encoding w={} h={} fps={} ceiling_mbps={:.1} codec={:?} backend={:?}",
            config.width,
            config.height,
            config.fps,
            config.configured_mbps,
            config.codec,
            backend
        );
        let exit = match backend {
            Backend::Open => run_open(shared, arrivals, config.clone(), &mut roster),
            Backend::Vendor => run_vendor(shared, arrivals, config.clone(), &mut roster),
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
                    // **Nobody could be served, and every guest was told.**
                    // The loop goes back to waiting rather than dying, but for
                    // the application this is the session not continuing.
                    if let Some(raise) = shared.raise.as_ref() {
                        raise.send(crate::admission::Event::Fatal { reason });
                    }
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
            Exit::Rediscover(_) | Exit::Reconfigure(_) => {
                if let Exit::Reconfigure(codec) = exit {
                    lowlat_common::log_info!(
                        "stream: reconfiguring codec={:?} -> {:?}",
                        config.codec,
                        codec
                    );
                    previous = Some(config.codec);
                    config.codec = codec;
                } else if let Exit::Rediscover(why) = exit {
                    // What it changed to is not carried: the loop reads it
                    // from the display on the way back in, which is the one
                    // place that knows.
                    lowlat_common::log_info!("stream: {why}, rebuilding");
                }
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
    /// What is being captured changed, and the pipeline has to be found again.
    ///
    /// **The encoder is built around one display and cannot follow one.** A
    /// size change leaves it coding the old size, so the new picture lands in
    /// a corner of a frame the rest of which never changes again; a display
    /// unplugged from this device leaves the controller scanning out the last
    /// thing it held, for ever. The reason is carried for the log because the
    /// two look identical from here: the loop hands the encoder back and the
    /// display is discovered again.
    Rediscover(&'static str),
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
            release(seat, shared);
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
    let ceiling = ceiling_step(budget);
    for guest in guests.iter_mut() {
        guest.set_rate(ceiling);
    }
}

/// Hold the sound device exactly while somebody is listening.
///
/// **Called from the loop that knows the room's size**, every pass, because a
/// room empties without anything else happening: no rebuild, no arrival, no
/// error. The loop that waits for a guest is not the loop that runs while
/// there are none, so a device owned by the first is never given back by the
/// second -- which is not hypothetical: it held a capture, and somebody's muted
/// speakers, across three sessions.
///
/// **The same is true of the device itself, one level down.** A capture ends
/// on its own thread -- a sound server that restarts takes it with it -- and
/// nothing announces that either, so whether it is still delivering is asked
/// here rather than assumed from having opened it once.
///
/// Cheap when nothing changed: one atomic, one uncontended lock and one load,
/// with nothing cloned or allocated on the pass that finds everything in
/// order. **The decision is recorded even when no device is configured**,
/// which is what makes it testable without one.
fn reconcile_sound(shared: &Arc<Shared>, listeners: usize, config: &Config) {
    let want = u32::from(listeners > 0 && shared.sound_on());
    if shared.sound_demand.swap(want, Ordering::AcqRel) != want {
        shared.sound_epoch.fetch_add(1, Ordering::Release);
    }
    let Ok(mut held) = shared.held_sound.lock() else {
        return;
    };
    if want == 0 {
        if held.take().is_some() {
            // Dropping it stops the capture, joins its thread, and puts the
            // speakers at the desk back.
            lowlat_common::log_info!("audio: nobody is listening, the sound device is back");
        }
        return;
    }
    let Some(audio) = config.audio.as_ref() else {
        return;
    };
    match held.as_mut() {
        // Held and delivering, which is every pass but two in a session. One
        // atomic load, and nothing is cloned.
        Some(sound) if sound.alive() => {}
        // **A capture ends on its own thread and says nothing to anybody.** A
        // sound server that restarts under a running session takes it with it,
        // and this pass is the only thing that can notice.
        Some(sound) => sound.regain(shared, audio),
        None => *held = Some(crate::audio::Sound::start(shared, audio)),
    }
}

/// The gate's view of a guest's ceiling.
#[allow(
    clippy::cast_possible_truncation,
    reason = "a rate in megabits, only used to pick one of three ceiling steps"
)]
fn ceiling_step(budget: &Budget) -> f32 {
    budget.ceiling() as f32
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
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
) -> Exit {
    // **The size the display settled on, before anything is built for it.**
    // Same rule as the vendor path: a display decides its own size and
    // everything downstream has to be told the same answer.
    let (width, height) = if config.display {
        match await_display(config.output.as_deref()) {
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
    shared.publish_picture(width, height);
    let config = Config {
        width,
        height,
        ..config
    };

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
    let Ok(mut encoder) = context.encoder(params, start_bps(&config)) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let mut desktop = if config.display {
        match crate::display::Display::open(
            ENCODE_DEPTH,
            config.output.as_deref(),
            |device, frame| crate::display::Display::register_open(device, &display, frame),
        ) {
            Ok(desktop) => Some(desktop),
            Err(error) => {
                lowlat_common::log_error!("stream: the display could not be opened, {error}");
                return Exit::Failed(status::CAPTURE_UNAVAILABLE);
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

fn run_vendor(
    shared: &Arc<Shared>,
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
        match await_display(config.output.as_deref()) {
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
    // **Said once the size is settled and before any guest is seated.** It is
    // the coordinate space a peer's absolute input is expressed in, so a guest
    // that seated against the configured numbers would place every position
    // scaled by the ratio between the two.
    shared.publish_picture(width, height);
    // **And the configuration follows it too, from here down.** Everything
    // below this line that asks the configuration how big the picture is has
    // to get the same answer the display gave, or it judges the picture
    // against a rectangle the picture is not in. The pointer did exactly that:
    // it was tested for being inside the stream against the configured size,
    // so on a display larger than it, every update from the part of the screen
    // beyond those bounds was dropped and a guest kept whatever shape it last
    // had.
    let config = Config {
        width,
        height,
        ..config
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
            bitrate_bps: start_bps(&config),
            min_qp: lowlat_encode::nvenc::DEFAULT_MIN_QP,
        },
    ) else {
        lowlat_common::log_error!("stream: encoder could not be configured");
        return Exit::Failed(status::ENCODER_UNAVAILABLE);
    };
    let mut desktop = if config.display {
        match crate::display::Display::open(
            lowlat_encode::nvenc::IN_FLIGHT,
            config.output.as_deref(),
            |device, frame| crate::display::Display::register_vendor(device, &encoder, frame),
        ) {
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
fn start_bps(config: &Config) -> u32 {
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

/// Hotspots learned per shape.
///
/// **Nothing reports a pointer's hotspot on this backend**, and it is load
/// bearing: the far side draws the picture against its own pointer, so the
/// offset it applies is the one we send. Sending zero draws every pointer down
/// and to the right of where it really is, by its own hotspot -- invisible on
/// an arrow, obvious on an I-beam or a crosshair whose point is in the middle.
///
/// So it is derived from the one thing this host knows that no driver does:
/// **we put the pointer there ourselves.** A guest commands a position, the
/// display draws the picture with its hotspot on that point, and the
/// difference between the command and the drawn corner is the hotspot exactly.
#[derive(Debug)]
struct Hotspots {
    /// Shape checksum, then its hotspot. Small: a desktop cycles through a
    /// handful of pointers and an entry that falls out is learned again.
    known: [(u32, u16, u16); HOTSPOTS],
    count: usize,
    next: usize,
    /// The command this last looked at, so a position that has not moved since
    /// is not sampled twice.
    seen: u64,
    /// Whether the command above still owes a sample.
    pending: bool,
}

/// Shapes whose hotspot is remembered at once.
const HOTSPOTS: usize = 16;

impl Hotspots {
    fn new() -> Self {
        Self {
            known: [(0, 0, 0); HOTSPOTS],
            count: 0,
            next: 0,
            seen: 0,
            pending: false,
        }
    }

    /// What is known about this shape, or zero.
    fn of(&self, checksum: u32) -> (u16, u16) {
        self.known
            .get(..self.count)
            .unwrap_or_default()
            .iter()
            .find(|(shape, _, _)| *shape == checksum)
            .map_or((0, 0), |(_, x, y)| (*x, *y))
    }

    /// Learn this shape's hotspot from a command and where the pointer was
    /// then drawn.
    ///
    /// **Refused unless the command lands inside the picture that was drawn**,
    /// which is the whole of the validation and most of what makes this safe:
    /// a sample taken while the pointer was moving, or one taken from a
    /// position somebody at the desk produced rather than a guest, does not
    /// land inside its own shape except by coincidence.
    fn learn(&mut self, checksum: u32, command: (u16, u16), drawn: (u16, u16, u16, u16)) -> bool {
        let (cx, cy) = command;
        let (x, y, width, height) = drawn;
        let (Some(hot_x), Some(hot_y)) = (cx.checked_sub(x), cy.checked_sub(y)) else {
            return false;
        };
        if hot_x >= width || hot_y >= height {
            return false;
        }
        if let Some(slot) = self
            .known
            .get_mut(..self.count)
            .unwrap_or_default()
            .iter_mut()
            .find(|(shape, _, _)| *shape == checksum)
        {
            *slot = (checksum, hot_x, hot_y);
            return true;
        }
        // **Oldest out.** A cache that filled and then refused to learn would
        // freeze whatever sixteen shapes it happened to see first.
        if let Some(slot) = self.known.get_mut(self.next) {
            *slot = (checksum, hot_x, hot_y);
        }
        self.next = (self.next + 1) % HOTSPOTS;
        self.count = (self.count + 1).min(HOTSPOTS);
        true
    }

    /// Take one command into account and answer what this shape's hotspot is.
    ///
    /// **Sampled once per command, on the read after it.** A pointer is drawn
    /// where it was told to be only once the display has caught up, and a
    /// sample taken on every read afterwards would keep re-deriving from a
    /// command that no longer says anything about where the pointer is -- which
    /// is what somebody at the desk moving the mouse produces.
    fn update(
        &mut self,
        command: Option<(u64, u16, u16)>,
        checksum: u32,
        drawn: (u16, u16, u16, u16),
    ) -> (u16, u16) {
        if let Some((count, cx, cy)) = command {
            if count != self.seen {
                self.seen = count;
                self.pending = true;
            } else if self.pending {
                self.pending = false;
                self.learn(checksum, (cx, cy), drawn);
            }
        }
        self.of(checksum)
    }
}

/// Where a pointer sits in the picture, or nothing when it is not in it.
///
/// **Skipped rather than clamped.** A pointer outside the picture is not this
/// stream's to describe, and a clamped one is a pointer parked against an edge
/// it is not at. The far side keeps drawing the last one it was told about,
/// which is what it should do.
///
/// **The bounds are the picture's, and getting them from anywhere else is a
/// bug that hides.** Tested against a smaller rectangle than the picture, every
/// pointer beyond it is dropped: the guest keeps whatever shape it last had
/// while the screen shows another, and it looks like a shape-detection fault
/// rather than a bounds one because it depends on where the pointer is.
fn within(x: i32, y: i32, width: u32, height: u32) -> Option<(u16, u16)> {
    // Negative is a pointer straddling the left or top edge: not representable
    // here, and outside by the same rule.
    let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
        return None;
    };
    (u32::from(x) < width && u32::from(y) < height).then_some((x, y))
}

/// Read the pointer and publish it for the guests to report.
///
/// **Skipped rather than clamped when the pointer is not on this stream.** A
/// pointer outside the picture is not this stream's to describe, and a clamped
/// one is a pointer parked against an edge it is not at. The far side keeps
/// drawing the last one it was told about, which is what it should do.
fn publish_pointer(
    shared: &Shared,
    display: Option<&mut crate::display::Display>,
    width: u32,
    height: u32,
    hotspots: &mut Hotspots,
    presence: &mut Presence,
    now_ms: f64,
) {
    let Some(desktop) = display else { return };
    let Some(seen) = desktop.pointer() else {
        // **Nothing is compositing a pointer, which usually means something
        // took it over.** Held for long enough, it is reported: a client turns
        // it into relative motion, which is what a game hiding the cursor to
        // aim actually wants. Below that it is one of the transients and
        // nothing is said, so the guest keeps drawing the last pointer where
        // it was.
        if let Some(state) = presence.observe(None, now_ms) {
            shared.publish_pointer(state, None);
        }
        return;
    };
    let Some((x, y)) = within(seen.x, seen.y, width, height) else {
        return;
    };
    // Derived from what a guest told the pointer to be, against where the
    // display then drew it. Zero until this shape has been seen once with a
    // command settled behind it.
    let (hot_x, hot_y) = hotspots.update(
        shared.commanded(),
        seen.checksum,
        (x, y, seen.width, seen.height),
    );
    let state = PointerState {
        x,
        y,
        hot_x,
        hot_y,
        width: seen.width,
        height: seen.height,
        checksum: seen.checksum,
        hidden: false,
    };
    let Some(state) = presence.observe(
        Some(Seen {
            state,
            looked: seen.looked,
        }),
        now_ms,
    ) else {
        return;
    };
    // The picture travels only when it is one nothing has been shown yet,
    // which can only be a read that looked.
    let image = (seen.fresh && seen.looked).then(|| desktop.pointer_image());
    shared.publish_pointer(state, image);
}

/// How long a guest waits for a display that is not scanning out.
///
/// Long enough that a screen blanked while nobody was looking has a chance to
/// come back when somebody arrives, short enough that a guest on a machine
/// with no display at all is told so rather than held.
const DISPLAY_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Which encoder can take frames from the device the display is on.
///
/// **A display and its encoder have to be on one device**, so this is read
/// from the display rather than configured. A machine with one card answers
/// the same thing every time and nothing about it is visible; a machine with
/// two answers differently depending on which screen is being captured, and
/// getting it wrong is an encoder that refuses every frame it is handed.
///
/// Anything that is not the vendor's own driver is served by the open stack,
/// including the open driver for the same hardware.
fn follow_display(config: &Config) -> Backend {
    if !config.display {
        return Backend::Open;
    }
    let driver = crate::display::Display::driver(config.output.as_deref());
    let backend = match driver.as_deref() {
        Some("nvidia") => Backend::Vendor,
        _ => Backend::Open,
    };
    lowlat_common::log_info!(
        "stream: the display is on {}, encoding with {backend:?}",
        driver.as_deref().unwrap_or("a device that did not say")
    );
    backend
}

/// The output most recently asked for, or nothing if none was.
///
/// **Only the last request matters.** A caller that changed its mind while the
/// loop was mid-frame meant the second answer, and rebuilding once per request
/// would capture each output it was briefly pointed at on the way.
fn requested(asked: &mpsc::Receiver<Option<String>>) -> Option<Option<String>> {
    let mut last = None;
    while let Ok(id) = asked.try_recv() {
        last = Some(id);
    }
    last
}

/// Wait for something to scan out, and report what shape it is.
fn await_display(wanted: Option<&str>) -> Option<(u32, u32)> {
    let began = lowlat_common::clock::Time::now();
    let mut said = false;
    loop {
        if let Some(size) = crate::display::Display::size_of_display(wanted) {
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
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool;
}

impl FromDevice for lowlat_encode::nvenc::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool {
        // **A registration made for the other backend is refused, not
        // reinterpreted.** It names an object this runtime has never heard of.
        let crate::display::Registration::Vendor { input, .. } = registration else {
            return false;
        };
        self.submit_registered(input, force_keyframe).is_ok()
    }
}

#[cfg(test)]
impl FromDevice for tests::Fake {
    fn submit_from_device(
        &mut self,
        _registration: &crate::display::Registration,
        _force_keyframe: bool,
    ) -> bool {
        false
    }
}

impl FromDevice for lowlat_encode::vaapi::Encoder<'_> {
    fn submit_from_device(
        &mut self,
        registration: &crate::display::Registration,
        force_keyframe: bool,
    ) -> bool {
        let crate::display::Registration::Open { surface } = registration else {
            return false;
        };
        self.submit_registered(*surface, force_keyframe).is_ok()
    }
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
fn encode_loop<E: Encoder + FromDevice>(
    shared: &Arc<Shared>,
    arrivals: &mpsc::Receiver<Join>,
    config: Config,
    roster: &mut Roster,
    encoder: &mut E,
    display: Option<&mut crate::display::Display>,
) -> Exit {
    let mut display = display;
    // Read once, on the way in: the loop only has to notice that it moved.
    let asked_at = shared.output_asked.load(Ordering::Acquire);
    // **Said once, here, where both backends meet.** The picture's size is
    // published before the display is opened, because the encoder is built
    // against it; where that picture sits in the desktop cannot be known until
    // the display exists, so it arrives one step later and the guests pick it
    // up on the pass after.
    shared.publish_place(display.as_ref().and_then(|desktop| desktop.place()));
    let selected = display.as_ref().and_then(|desktop| desktop.selected());
    shared.publish_captured(selected);
    // **Announced from the one place that knows both halves.** The size and
    // the output are what a peer is told it is watching and what its absolute
    // input is expressed against, and an application left to notice by polling
    // is one asking a question this loop already answered.
    if let Some(raise) = shared.raise.as_ref() {
        raise.send(crate::admission::Event::CaptureChanged {
            width: config.width,
            height: config.height,
            output: selected.unwrap_or_default().to_string(),
        });
    }
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
    let mut live = shared
        .video
        .lock()
        .map_or_else(|held| *held.into_inner(), |held| *held);
    let mut video_seen = shared.video_asked.load(Ordering::Acquire);
    let mut interval_ms = 1000.0 / f64::from(live.fps.max(1));
    let mut next_frame_ms = 0.0f64;
    let mut previous_submit: Option<lowlat_common::clock::Time> = None;
    let mut force_keyframe = false;
    // When a picture was last actually submitted, which is what the heartbeat
    // measures from. Starts long ago, so the first frame is never suppressed.
    let mut forced_ms = f64::MIN;
    let mut suppressed = 0u32;
    let mut refreshes = Refreshes::default();
    let mut since_report = 0u32;
    let mut failures = 0u32;
    // Whether the last pass found the display dark.
    let mut held = false;
    // When the pointer was last looked at, and what is known about where each
    // shape points.
    let mut pointer_ms = -POINTER_MS;
    let mut hotspots = Hotspots::new();
    let mut presence = Presence::default();
    let mut attached_ms = 0.0f64;

    loop {
        if shared.stopping.load(Ordering::Acquire) != 0 {
            return Exit::Stopped;
        }

        let moved = admit_and_retire(shared, arrivals, &config, active, guests, controllers);
        if moved {
            rebind(&mut budget, active.len(), guests, controllers);
        }
        // **What sound costs is taken off the picture's ceiling**, and it
        // changes without a guest arriving: a peer declares the encoding it
        // wants a pass or two after it is seated. Compared rather than
        // recomputed into place, because moving a ceiling reconfigures every
        // controller.
        let sound_mbps = if shared.sound_on() {
            shared.audio_mbps(shared.sound_kbps())
        } else {
            0.0
        };
        if (sound_mbps - budget.audio_mbps()).abs() > crate::rate::DEADBAND_MBPS {
            budget.set_audio(sound_mbps, controllers);
            let ceiling = ceiling_step(&budget);
            for guest in guests.iter_mut() {
                guest.set_rate(ceiling);
            }
            lowlat_common::log_info!(
                "stream: sound costs {sound_mbps:.2} Mibit/s, the picture may use {:.2}",
                budget.ceiling()
            );
        }
        // **Every pass, because the room's size changes here and nowhere
        // else.** Switching sound off while a host runs arrives the same way.
        reconcile_sound(shared, active.len(), &config);
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
        // **A declaration is a reason on its own, not only a request.** A peer
        // states what it can decode when it joins, and a host that acted only
        // on an explicit reinitialization left a guest that asked for one
        // codec being sent the other until it went away and came back. What
        // every seated guest can decode is the whole of the decision, so a
        // change in it is the trigger.
        let asked = consensus(shared, active);
        let wanted = if asked & lowlat_core::init::FLAG_HEVC != 0 {
            Codec::H265
        } else {
            Codec::H264
        };
        let disagrees = asked != 0 && wanted != config.codec;
        if reconfigure_asked(shared, active) || disagrees {
            if asked & NOT_EMITTED != 0 {
                lowlat_common::log_warn!(
                    "stream: guests asked for flags={:#x} and this pipeline emits 8-bit 4:2:0, \
                     so {:#x} of it is not granted",
                    asked,
                    asked & NOT_EMITTED
                );
            }
            // **The whole decision on one line.** What every seat agreed on,
            // how many seats that was over, what settled the codec, and which
            // of the two reasons brought us here: a request that changes
            // nothing, a request that is outvoted, and a declaration nobody
            // asked about look identical from the outside otherwise.
            lowlat_common::log_info!(
                "stream: reinit {}, consensus={:#x} over {} seat(s), codec={:?} -> {:?}",
                if disagrees { "by declaration" } else { "asked" },
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
            refreshes.reinit = refreshes.reinit.saturating_add(1);
        }

        // **Only while the encoder holds something.** A pass with nothing in
        // flight has nothing to collect, and asking anyway is a driver round
        // trip for an answer that cannot have changed. `in_flight` is exactly
        // the set of submitted pictures: pushed on submit, popped as each one
        // comes back.
        //
        // **A stopped encoder is still caught**, because one that has stopped
        // answering is one whose submissions never come back, which leaves
        // this non-empty and keeps the poll running.
        let pending = !in_flight.is_empty();
        // Anything the hardware finished while this loop was elsewhere. Never
        // waits: a picture that is not ready costs a driver round trip and the
        // loop goes on to the frame clock.
        let collected = if pending {
            collect(
                shared,
                encoder,
                gate,
                active,
                guests,
                &mut in_flight,
                &mut stages,
                started,
                &mut refreshes,
            )
        } else {
            Collected {
                keyframe: false,
                failed: false,
            }
        };
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

        // **Is anything still plugged into the device being captured?** A
        // controller whose connector has gone keeps scanning out the last
        // picture it held, so nothing above this notices: every read succeeds
        // and the stream carries a desktop that stopped changing. Asked on its
        // own slow cadence, because it walks every connector.
        if now_ms - attached_ms >= ATTACHED_MS {
            attached_ms = now_ms;
            if display.as_deref().is_some_and(|d| !d.attached()) {
                return Exit::Rediscover("nothing is plugged into the device being captured");
            }
        }
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
                    Some(read.unwrap_or(crate::display::Acquired {
                        at: began,
                        // **A display that could not be read holds its last
                        // picture, and holding it is not a change.** Calling a
                        // failed read new would send the same frame at the full
                        // rate for as long as the display is dark.
                        changed: false,
                    }))
                }
                None => None,
            };
            // **The pointer, on its own cadence and after the picture.** It
            // is read from the same device and the same thread as the frame,
            // which is what the state it reports is a property of; a thread
            // outside this one sees another seat or nothing at all.
            if now_ms - pointer_ms >= POINTER_MS {
                pointer_ms = now_ms;
                publish_pointer(
                    shared,
                    display.as_deref_mut(),
                    config.width,
                    config.height,
                    &mut hotspots,
                    &mut presence,
                    now_ms,
                );
            }

            // **Before anything is encoded from it.** A frame taken at the
            // new size and coded by an encoder built for the old one is the
            // picture in a corner of a stale frame, which is what a peer sees
            // until it is told otherwise.
            if display.as_deref_mut().is_some_and(Display::take_resize) {
                return Exit::Rediscover("the display changed size");
            }
            // **The same rebuild, for the same reason.** A picture from another
            // output is no more absorbable than one of another size: the
            // encoder and the conversion target are built around the one that
            // was there when they were made.
            if shared.output_asked.load(Ordering::Acquire) != asked_at {
                return Exit::Rediscover("a different output was asked for");
            }
            // **Applied without rebuilding, which is what separates these from
            // an output change.** The bitrate re-bases the budget and reaches
            // the encoder through the reconfigure the rate loop already does;
            // the frame rate changes the pacing from the next frame on. Read
            // only when the counter moved, so an unchanged stream pays one
            // load a pass.
            if take_live_video(shared, &mut video_seen, &mut live) {
                interval_ms = 1000.0 / f64::from(live.fps.max(1));
                budget.reconfigure(live.bitrate_mbps, live.min_mbps, controllers);
                lowlat_common::log_info!(
                    "stream: live video change, fps={} bitrate={:.1} floor={:.1} full_fps={}",
                    live.fps,
                    live.bitrate_mbps,
                    live.min_mbps,
                    u8::from(live.full_fps)
                );
            }

            let synthetic = if display.is_none() {
                Some(source.acquire())
            } else {
                None
            };
            let captured_at = ready
                .map(|got| got.at)
                .or_else(|| synthetic.as_ref().map(|frame| frame.captured_at))
                .unwrap_or(began);
            // **The generator has no summary and its frames all differ**, so a
            // stream without a display is untouched by any of this.
            let changed = ready.is_none_or(|got| got.changed);
            // **What the acquire cost, not when the picture is stamped.** A
            // display reports the moment it began, because that is when the
            // picture on screen was the one being taken and it is what the
            // latency a peer is told counts from; measuring the stage against
            // that stamp compares a clock reading with itself and reports
            // zero, which is what it did, hiding the capture and the colour
            // conversion inside a figure nobody could break down.
            stages.acquire.record(lowlat_common::clock::diff_ms(
                began,
                lowlat_common::clock::Time::now(),
            ));

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
                if gate.request_keyframe(asked_at) == Keyframe::Request {
                    force_keyframe = true;
                    refreshes.asked = refreshes.asked.saturating_add(1);
                }
            }

            // **A picture nobody needs is not submitted.** Everything below
            // this -- the encoder, the packetisation, the encryption, the wire
            // -- exists to carry a difference, and there is none.
            //
            // Three things override that, each a case where an identical
            // picture is still owed to somebody:
            //
            // - **a refresh is owed.** A guest that has fallen out of the
            //   reference chain, or one that just joined, needs a picture with
            //   no history behind it, and a screen that is not moving is
            //   exactly the case where it would otherwise wait forever.
            // - **the seats changed.** Someone arriving or leaving alters who
            //   the next frame is for, and an arrival has received nothing.
            // - **the heartbeat is due.** See `HEARTBEAT_MS`: it bounds how
            //   long any mistake in this reasoning can leave a screen frozen.
            let send = must_send(changed, force_keyframe, moved, now_ms - forced_ms);
            if send {
                forced_ms = now_ms;
            } else {
                suppressed = suppressed.saturating_add(1);
                shared.suppressed.fetch_add(1, Ordering::Relaxed);
            }

            let submitted_at = lowlat_common::clock::Time::now();
            // Read before the submit, because a picture that goes in clears it.
            let refreshing = force_keyframe;
            // **Not an early return.** The wait at the foot of the loop is what
            // keeps this thread off the processor, and skipping it to skip a
            // frame would turn a suppressed stream into a spinning one.
            let queued = match (send, &synthetic, display.as_deref_mut()) {
                (false, _, _) => false,
                (true, Some(frame), _) => encoder.submit(frame, force_keyframe).is_ok(),
                (true, None, Some(desktop)) => match desktop.presented() {
                    Some(input) => encoder.submit_from_device(input, force_keyframe),
                    None => false,
                },
                (true, None, None) => false,
            };
            if queued {
                if refreshing {
                    refreshes.sent = refreshes.sent.saturating_add(1);
                }
                force_keyframe = false;
                if let Some(previous) = previous_submit {
                    stages
                        .interval
                        .record(lowlat_common::clock::diff_ms(previous, submitted_at));
                }
                previous_submit = Some(submitted_at);
                in_flight.push_back((captured_at, submitted_at));
            }
            // A refusal is the encoder holding as many pictures as it will:
            // back pressure rather than a fault, and the next frame goes in
            // once a collect has made room.

            // **Counted per frame considered, not per frame sent.** A
            // suppressed stream submits once a second, and a report that waited
            // for six hundred of those would arrive every ten minutes -- so the
            // one number that says whether suppression is working would be the
            // hardest to see.
            since_report += 1;
            {
                if since_report >= REPORT_FRAMES {
                    since_report = 0;
                    let report = stages.report();
                    shared.timing.publish(&report);
                    // **Where the time goes, not just how much of it there
                    // is.** The figure a peer is told is one number covering
                    // capture, conversion, submission and the wait for the
                    // hardware, and the four move for different reasons: a
                    // slower device, a larger picture, and a stage that
                    // blocks all read the same from outside.
                    lowlat_common::log_info!(
                        "stream: stages ms p50/p99 acquire={:.3}/{:.3} encode={:.3}/{:.3} \
                         publish={:.3}/{:.3} interval={:.3}/{:.3}",
                        report.acquire.p50,
                        report.acquire.p99,
                        report.encode.p50,
                        report.encode.p99,
                        report.publish.p50,
                        report.publish.p99,
                        report.interval.p50,
                        report.interval.p99,
                    );
                    // **Separate from the stages line because it is a count
                    // over the window, not a distribution.** The stages are a
                    // rolling percentile and this resets, so a rate can be
                    // read off it directly.
                    lowlat_common::log_info!(
                        "stream: refresh over {} frames sent={} no_slot={} too_large={} \
                         no_room={} asked={} reinit={} starved={}",
                        REPORT_FRAMES,
                        refreshes.sent,
                        refreshes.no_slot,
                        refreshes.too_large,
                        refreshes.no_room,
                        refreshes.asked,
                        refreshes.reinit,
                        refreshes.starved,
                    );
                    // **The number that says whether any of this is working.**
                    // A static desktop should suppress nearly every frame and a
                    // moving one nearly none, so a figure that never moves in
                    // either direction is the interesting failure.
                    lowlat_common::log_info!(
                        "stream: suppressed {suppressed} of {} frames since the last report",
                        REPORT_FRAMES
                    );
                    suppressed = 0;
                    shared.refreshes.publish(&refreshes);
                    refreshes = Refreshes::default();
                }
            }
        }

        // Wait for the sooner of the next thing this loop owes anybody. **A
        // millisecond is the floor**, because anything shorter asked of the
        // scheduler is a busy wait with extra steps; the final approach to a
        // frame deadline is the one place an accurate landing is worth its
        // spin, and it happens once per frame rather than once per poll.
        //
        // **The poll cadence applies only while something is in flight.**
        // Without that this waits a millisecond at a time whatever is
        // happening, which on an untouched desktop is roughly seven hundred
        // wakeups a second to discover that nothing has changed.
        // **Asked again here, not reused from the top of the pass.** A picture
        // was very likely submitted since then, and it is the pass that submits
        // one which most needs to poll: taking the answer from before the
        // submit sleeps until the next frame instead, so every picture waits a
        // whole frame interval to be collected and the latency reported to a
        // peer grows by one.
        let waiting = !in_flight.is_empty();
        let now_ms = lowlat_common::clock::elapsed_ms(started);
        let mut until = next_frame_ms - now_ms;
        if !waiting {
            // The connector check is the only other thing owed on a pass, and
            // on a slow frame rate it can fall due first.
            until = until.min(attached_ms + ATTACHED_MS - now_ms);
        }
        if waiting && until > POLL_MS * 2.0 {
            std::thread::sleep(COLLECT_WAIT);
        } else if until > 0.0 {
            lowlat_common::clock::precise_sleep(std::time::Duration::from_secs_f64(until / 1000.0));
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

/// Take a live video change if one was asked for.
///
/// **Extracted so the decision can be tested without a display.** The loop it
/// belongs to needs a render node and does not run by default, and a check that
/// never runs proves nothing: pinning this counter so no change was ever seen
/// passed the entire suite, because the only test watching read the settings
/// back out of the cell rather than out of the loop.
///
/// Answers whether anything moved, so the caller recomputes only then.
fn take_live_video(shared: &Shared, seen: &mut u32, live: &mut LiveVideo) -> bool {
    let asked = shared.video_asked.load(Ordering::Acquire);
    if asked == *seen {
        return false;
    }
    *seen = asked;
    *live = shared
        .video
        .lock()
        .map_or_else(|held| *held.into_inner(), |held| *held);
    true
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
            // **A seat that has declared nothing is not a vote.** A guest takes
            // its seat the moment it is streamable and its declaration reaches
            // the seat a pass later, so counting that gap as "can decode
            // nothing" drags the stream down to the base codec and back again.
            // Every real declaration carries the base flag, so zero is only
            // ever the gap.
            if declared == 0 {
                continue;
            }
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
    config: &Config,
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
            release(seat, shared);
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
        controllers.push(Controller::new(
            config.cg_level,
            config.min_mbps,
            config.configured_mbps,
        ));
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
        release(seat, shared);
        seat.state.store(seat_state::FREE, Ordering::Release);
        active.swap_remove(at);
        guests.swap_remove(at);
        controllers.swap_remove(at);
        moved = true;
    }

    moved
}

/// Empty a seat's ring, giving every queued frame back to the pool.
fn release(seat: &Seat, shared: &Shared) {
    // **Both rings, and the wake.** A slot left in either pool never comes
    // back, and a wake left standing points at a descriptor the departing
    // guest is about to close.
    while let Some(index) = seat.audio.pop() {
        drop(shared.audio.claim(index));
    }
    if let Ok(mut held) = seat.audio_wake.lock() {
        *held = None;
    }
    let pool = &shared.pool;
    while let Some(index) = seat.ring.pop() {
        drop(pool.claim(index));
    }
}

/// Refreshes over one report window, by what asked for them.
///
/// **A refresh costs a picture with no history behind it**, so a stream that
/// sends them often spends most of its rate on recovery rather than on
/// content, and nothing about the frame rate or the encode time says so. The
/// causes are separated because they call for different answers: a starved
/// pool is back pressure, a refused room test is a peer that cannot keep up,
/// and a reinitialization is neither.
///
/// **`sent` is not the sum of the rest.** Several causes landing in one frame
/// interval produce one refresh, and the throttle in [`gate`] refuses most of
/// what is asked for, so `sent` is what reached the wire and the others are
/// what wanted to.
#[derive(Debug, Clone, Copy, Default)]
struct Refreshes {
    /// Pictures submitted asking the encoder for one.
    sent: u32,
    /// Granted because every pool slot was still held.
    no_slot: u32,
    /// Granted because a coded frame did not fit the slot it was given.
    too_large: u32,
    /// Granted because the gate refused a guest room for the frame.
    no_room: u32,
    /// Granted because a peer could not decode what it was sent.
    asked: u32,
    /// Forced by a stream reinitialization. **Not throttled**, unlike every
    /// other row here, so it is the one that can run away.
    reinit: u32,
    /// Passes that found no pool slot, whether or not one was granted. The
    /// pressure behind `no_slot`, which the throttle otherwise hides.
    starved: u32,
}

impl Refreshes {
    /// **Order is the wire between [`RefreshCells`] and this**, so the two
    /// conversions are written together and a field added to one without the
    /// other does not compile.
    fn counts(&self) -> [u32; 7] {
        [
            self.sent,
            self.no_slot,
            self.too_large,
            self.no_room,
            self.asked,
            self.reinit,
            self.starved,
        ]
    }

    #[cfg(all(test, not(loom)))]
    fn from_counts(counts: [u32; 7]) -> Self {
        let [sent, no_slot, too_large, no_room, asked, reinit, starved] = counts;
        Self {
            sent,
            no_slot,
            too_large,
            no_room,
            asked,
            reinit,
            starved,
        }
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
    refreshes: &mut Refreshes,
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
                    refreshes.starved = refreshes.starved.saturating_add(1);
                    if gate.request_keyframe(now_ms) == Keyframe::Request {
                        wanted = true;
                        refreshes.no_slot = refreshes.no_slot.saturating_add(1);
                    }
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
                    if gate.request_keyframe(now_ms) == Keyframe::Request {
                        wanted = true;
                        refreshes.too_large = refreshes.too_large.saturating_add(1);
                    }
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
                if request == Keyframe::Request {
                    wanted = true;
                    refreshes.no_room = refreshes.no_room.saturating_add(1);
                }
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

    /// **A live change reaches the loop, and only when one was asked for.**
    ///
    /// The loop that consumes this needs a render node and does not run by
    /// default, so the decision is tested here instead. It is not hypothetical:
    /// pinning the counter so no change was ever seen passed the whole suite,
    /// because the only test watching read the settings back out of the cell
    /// the setter had just written.
    #[test]
    fn a_live_video_change_is_taken_once_and_then_not_again() {
        let stream = Stream::start(Config {
            audio: None,
            audio_on: false,
            accept_microphone: false,
            audio_kbps: 128,
            allow_raw_audio: false,
            output: None,
            display: false,
            width: 320,
            height: 240,
            fps: 60,
            cg_level: 1,
            full_fps: false,
            codec: Codec::H264,
            backend: Some(Backend::Open),
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
        });
        let shared = Arc::clone(&stream.shared);
        let mut seen = shared.video_asked.load(Ordering::Acquire);
        let mut live = stream.video();
        assert!(
            !take_live_video(&shared, &mut seen, &mut live),
            "a change was taken when none was asked for"
        );

        let wanted = LiveVideo {
            fps: 30,
            bitrate_mbps: 4.0,
            min_mbps: 2.0,
            full_fps: false,
        };
        stream.set_video(wanted);
        assert!(
            take_live_video(&shared, &mut seen, &mut live),
            "a change was asked for and the loop would not have seen it"
        );
        assert_eq!(live, wanted);

        // And exactly once: a second pass over an unchanged counter must not
        // re-apply, or every frame would re-base the budget.
        assert!(!take_live_video(&shared, &mut seen, &mut live));
    }
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
                audio: None,
                audio_on: false,
                accept_microphone: false,
                audio_kbps: 128,
                allow_raw_audio: false,
                output: None,
                display: false,
                width: 320,
                height: 240,
                fps: 240,
                codec: Codec::H264,
                backend: Some(Backend::Open),
                configured_mbps: 10.0,
                min_mbps: 1.0,
                rotation: lowlat_core::video::Rotation::None,
                detail_rows: 0,
                full_fps: false,
                cg_level: 1,
            };
            let shared = Arc::new(Shared {
                audio: crate::audio::pool(),
                held_sound: std::sync::Mutex::new(None),
                sound_demand: AtomicU32::new(0),
                sound_epoch: AtomicU32::new(0),
                sound: SoundCells::new(&test_config(Codec::H264)),
                sound_live: Arc::new(lowlat_audio::Wanted::default()),
                seats: core::array::from_fn(|_| Seat::new()),
                pool: Pool::new(slots, max_frame_bytes()),
                encode_us: AtomicU32::new(0),
                timing: TimingCells::default(),
                refreshes: RefreshCells::default(),
                suppressed: AtomicU32::new(0),
                picture: AtomicU32::new(0),
                place_rect: AtomicU64::new(0),
                place_desktop: AtomicU32::new(0),
                commanded: AtomicU64::new(0),
                cursor: CursorCell::default(),
                stopping: AtomicU32::new(0),
                captured: AtomicU32::new(0),
                output_asked: AtomicU32::new(0),
                raise: None,
                video_asked: AtomicU32::new(0),
                video: std::sync::Mutex::new(LiveVideo::default()),
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
                    outputs: None,
                    thread: Some(thread),
                },
                forced,
            }
        }

        fn seat(&self) -> SeatHold {
            let wake = lowlat_net::Wake::new().expect("wake");
            self.stream
                .seats()
                .take(
                    wake.handle().expect("handle"),
                    wake.handle().expect("a second handle"),
                )
                .expect("a free seat")
        }
    }

    /// A stream whose loop this thread drives, so a return can be observed.
    fn parked() -> (Arc<Shared>, Stream, mpsc::Receiver<Join>) {
        let shared = Arc::new(Shared {
            audio: crate::audio::pool(),
            sound: SoundCells::new(&test_config(Codec::H264)),
            held_sound: std::sync::Mutex::new(None),
            sound_demand: AtomicU32::new(0),
            sound_epoch: AtomicU32::new(0),
            sound_live: Arc::new(lowlat_audio::Wanted::default()),
            seats: core::array::from_fn(|_| Seat::new()),
            pool: Pool::new(POOL_SLOTS, max_frame_bytes()),
            encode_us: AtomicU32::new(0),
            timing: TimingCells::default(),
            refreshes: RefreshCells::default(),
            suppressed: AtomicU32::new(0),
            picture: AtomicU32::new(0),
            place_rect: AtomicU64::new(0),
            place_desktop: AtomicU32::new(0),
            commanded: AtomicU64::new(0),
            cursor: CursorCell::default(),
            stopping: AtomicU32::new(0),
            captured: AtomicU32::new(0),
            output_asked: AtomicU32::new(0),
            raise: None,
            video_asked: AtomicU32::new(0),
            video: std::sync::Mutex::new(LiveVideo::default()),
            epoch: AtomicU32::new(0),
        });
        let (joins, arrivals) = mpsc::channel();
        let stream = Stream {
            shared: Arc::clone(&shared),
            joins: Some(joins),
            outputs: None,
            thread: None,
        };
        (shared, stream, arrivals)
    }

    fn test_config(codec: Codec) -> Config {
        Config {
            audio: None,
            audio_on: false,
            accept_microphone: false,
            audio_kbps: 128,
            allow_raw_audio: false,
            output: None,
            display: false,
            width: 320,
            height: 240,
            fps: 240,
            codec,
            backend: Some(Backend::Open),
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
            full_fps: false,
            cg_level: 1,
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
    /// **A read that did not look at the pixels is not evidence of a pointer.**
    /// The picture is read on a cadence and the reads between report the one
    /// already held; counting those as a pointer still being drawn resets the
    /// timer three times out of four, so it never expires and a game that hid
    /// the cursor never reaches the guest.
    #[test]
    fn only_a_read_that_looked_clears_the_wait() {
        let seen = PointerState {
            x: 10,
            y: 20,
            ..PointerState::default()
        };
        let mut presence = Presence::default();
        assert_eq!(
            presence.observe(
                Some(Seen {
                    state: seen,
                    looked: true
                }),
                0.0
            ),
            Some(seen)
        );

        // Nothing drawn, then the reads in between that did not look, then
        // nothing drawn again -- which is exactly the pattern a game that
        // hides the cursor produces, three ticks in four.
        let blind = Some(Seen {
            state: seen,
            looked: false,
        });
        assert_eq!(
            presence.observe(None, 100.0),
            None,
            "the wait did not start"
        );
        for at in [118.0, 136.0, 154.0] {
            assert_eq!(
                presence.observe(blind, at),
                None,
                "a read that did not look spoke for the pointer"
            );
        }
        assert_eq!(presence.observe(None, 172.0), None);
        for at in [190.0, 208.0, 226.0] {
            assert_eq!(presence.observe(blind, at), None);
        }

        let held = presence
            .observe(None, 100.0 + HIDDEN_AFTER_MS)
            .expect("the deadline never arrived");
        assert!(held.hidden, "the wait expired into the wrong answer");

        // And a read that did look, with a pointer in it, ends it at once.
        assert_eq!(
            presence.observe(
                Some(Seen {
                    state: seen,
                    looked: true
                }),
                999.0
            ),
            Some(seen)
        );
        assert!(!presence.waiting());
    }

    /// **The plane emptying is a noisy version of "an application took the
    /// pointer".** It is that when a game hides the cursor to aim, and it is
    /// not when a pointer grew past what the plane can carry or when a display
    /// mode has just changed. Both of those pass and an application holding
    /// the pointer does not, so the answer is held before it is believed --
    /// and released the moment the pointer is back, because the cost of being
    /// late on that edge is a guest that cannot see its own pointer.
    #[test]
    fn nothing_drawn_is_believed_only_after_it_persists() {
        // **Nothing drawn before anything ever was says nothing.** A stream
        // starting onto an idle desktop whose compositor is not using the
        // pointer plane would otherwise tell a guest its pointer had been
        // taken over before it had been shown one.
        let mut fresh = Presence::default();
        assert_eq!(fresh.dark(1000.0), None);
        assert_eq!(fresh.dark(1000.0 + HIDDEN_AFTER_MS * 10.0), None);

        let mut presence = Presence {
            dark_since: None,
            last: Some(PointerState {
                x: 10,
                y: 20,
                ..PointerState::default()
            }),
        };

        // A transient blank says nothing at all.
        assert_eq!(presence.dark(1000.0), None);
        assert_eq!(presence.dark(1000.0 + HIDDEN_AFTER_MS - 1.0), None);

        // Held, it is reported, and it carries the last position because the
        // plane that holds the position is the one that went away.
        let held = presence.dark(1000.0 + HIDDEN_AFTER_MS).expect("a report");
        assert!(held.hidden);
        assert_eq!((held.x, held.y), (10, 20));

        // And a pointer coming back clears it with no delay at all.
        presence.lit(PointerState {
            x: 30,
            y: 40,
            ..PointerState::default()
        });
        assert_eq!(presence.dark(2000.0), None, "the clock did not restart");
        assert_eq!(
            presence.dark(2000.0 + HIDDEN_AFTER_MS).map(|s| (s.x, s.y)),
            Some((30, 40)),
            "and it reports where the pointer was last seen"
        );
    }

    /// A request carries a name and says that it did.
    ///
    /// **The ordering between the two is not what this checks**, and it was
    /// written as though it were: the name is put on the channel before the
    /// counter moves, because the loop reads the counter and only then takes
    /// the name -- but a single-threaded test sees both after the fact whichever
    /// order they happened in, and this one passed with them reversed. The
    /// ordering lives on the implementation, where it can be read; what is
    /// checked here is that both halves happen at all, and that a caller which
    /// changed its mind is answered with its last word.
    #[test]
    fn a_request_carries_a_name_and_the_last_one_wins() {
        let shared = Arc::new(Shared {
            audio: crate::audio::pool(),
            sound: SoundCells::new(&test_config(Codec::H264)),
            held_sound: std::sync::Mutex::new(None),
            sound_demand: AtomicU32::new(0),
            sound_epoch: AtomicU32::new(0),
            sound_live: Arc::new(lowlat_audio::Wanted::default()),
            seats: core::array::from_fn(|_| Seat::new()),
            pool: Pool::new(2, 64),
            encode_us: AtomicU32::new(0),
            timing: TimingCells::default(),
            refreshes: RefreshCells::default(),
            suppressed: AtomicU32::new(0),
            picture: AtomicU32::new(0),
            place_rect: AtomicU64::new(0),
            place_desktop: AtomicU32::new(0),
            commanded: AtomicU64::new(0),
            cursor: CursorCell::default(),
            stopping: AtomicU32::new(0),
            captured: AtomicU32::new(0),
            output_asked: AtomicU32::new(0),
            raise: None,
            video_asked: AtomicU32::new(0),
            video: std::sync::Mutex::new(LiveVideo::default()),
            epoch: AtomicU32::new(0),
        });
        let (outputs, asked) = mpsc::channel();
        let stream = Stream {
            shared: Arc::clone(&shared),
            joins: None,
            outputs: Some(outputs),
            thread: None,
        };

        assert_eq!(shared.output_asked.load(Ordering::Acquire), 0);
        stream.select_output(Some("card0:DP-2".to_string()));
        assert_eq!(
            asked.try_recv().expect("the name never arrived"),
            Some("card0:DP-2".to_string()),
            "the counter moved without a name behind it"
        );
        assert_eq!(shared.output_asked.load(Ordering::Acquire), 1);
        assert_eq!(
            requested(&asked),
            None,
            "a drained channel asks for nothing"
        );

        // **Asking twice is one rebuild's worth of work, not two.** A caller
        // that changed its mind gets what it asked for second, and the output
        // it was briefly pointed at is never captured.
        stream.select_output(None);
        stream.select_output(Some("card1:HDMI-A-1".to_string()));
        assert_eq!(
            requested(&asked),
            Some(Some("card1:HDMI-A-1".to_string())),
            "a superseded request was rebuilt onto"
        );
        assert_eq!(shared.output_asked.load(Ordering::Acquire), 3);
    }

    /// **The hotspot comes from the one thing this host knows and no driver
    /// does: it put the pointer there itself.** A guest commands a position,
    /// the display draws the shape with its point on it, and the difference
    /// between the command and the drawn corner is the hotspot exactly.
    #[test]
    fn a_hotspot_is_the_difference_between_the_command_and_the_drawing() {
        let mut hotspots = Hotspots::new();
        let arrow = 0xAAAA_AAAA;
        let drawn = (100u16, 100u16, 21u16, 24u16);

        // A command nothing has settled behind yet teaches nothing.
        assert_eq!(hotspots.update(Some((1, 110, 108)), arrow, drawn), (0, 0));
        // The read after it is where the display has caught up.
        assert_eq!(hotspots.update(Some((1, 110, 108)), arrow, drawn), (10, 8));
        // And it is remembered for that shape without another command.
        assert_eq!(hotspots.update(None, arrow, drawn), (10, 8));
        // A shape nothing has been learned about is still zero, which is
        // wrong by a hotspot rather than wrong by an unknown.
        assert_eq!(hotspots.update(None, 0xBBBB_BBBB, drawn), (0, 0));
    }

    /// **A sample that does not land inside the shape it is meant to be in is
    /// refused.** That is the whole of the validation, and it is what keeps
    /// somebody at the desk moving the mouse from teaching a hotspot: a
    /// position that was not ours does not land inside its own picture except
    /// by coincidence.
    #[test]
    fn a_hotspot_outside_its_own_shape_is_refused() {
        let mut hotspots = Hotspots::new();
        let shape = 0xCCCC_CCCC;
        let drawn = (100u16, 100u16, 21u16, 24u16);

        // Past the right edge of the picture.
        hotspots.update(Some((1, 500, 108)), shape, drawn);
        assert_eq!(hotspots.update(Some((1, 500, 108)), shape, drawn), (0, 0));

        // Before its corner, which is a command from above or to the left of
        // where the pointer was drawn.
        let mut hotspots = Hotspots::new();
        hotspots.update(Some((1, 90, 108)), shape, drawn);
        assert_eq!(hotspots.update(Some((1, 90, 108)), shape, drawn), (0, 0));
    }

    /// The cache holds a handful of shapes and keeps learning past that, or a
    /// desktop that cycles more pointers than it holds would freeze on the
    /// first ones it saw.
    #[test]
    fn a_full_hotspot_cache_keeps_learning() {
        let mut hotspots = Hotspots::new();
        let drawn = (100u16, 100u16, 21u16, 24u16);
        let over = u32::try_from(HOTSPOTS).expect("a small cache") + 4;
        for shape in 0..over {
            let command = Some((u64::from(shape) + 1, 105, 104));
            hotspots.update(command, shape, drawn);
            assert_eq!(hotspots.update(command, shape, drawn), (5, 4), "at {shape}");
        }
    }

    /// **A pointer is judged against the picture, not against what the host
    /// was configured with.** A 2560x1440 display tested against 1920x1080
    /// drops every update from the right third and the bottom quarter of the
    /// screen, and the guest keeps whatever shape it last had while the screen
    /// shows another. It reads as a shape-detection fault because it depends
    /// on where the pointer is, which is why it survived a cursor that was
    /// otherwise working.
    #[test]
    fn a_pointer_is_judged_against_the_whole_picture() {
        assert_eq!(within(100, 100, 2560, 1440), Some((100, 100)));
        assert_eq!(
            within(2000, 1200, 2560, 1440),
            Some((2000, 1200)),
            "the part of a larger display that smaller bounds would drop"
        );
        assert_eq!(within(2000, 1200, 1920, 1080), None, "genuinely outside");

        // A pointer straddling an edge is outside, not wrapped to the far one.
        assert_eq!(within(-1, 5, 2560, 1440), None);
        assert_eq!(within(5, -1, 2560, 1440), None);
        assert_eq!(within(2560, 0, 2560, 1440), None, "one past the last pixel");
    }

    /// **The size a peer is told is the size its input comes back in.** A
    /// display decides its own, and a guest that described the stream with the
    /// configured numbers instead put every absolute position through the
    /// ratio between the two: measured live, a 2560x1440 display described as
    /// 1920x1080 reached the right edge of the screen three quarters of the
    /// way across the picture.
    #[test]
    fn the_settled_picture_size_reaches_a_guest_exactly() {
        let (shared, stream, _arrivals) = parked();
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(
                wake.handle().expect("handle"),
                wake.handle().expect("a second handle"),
            )
            .expect("a free seat");

        assert_eq!(seat.picture(), None, "a size nothing has settled on yet");
        shared.publish_picture(2560, 1440);
        assert_eq!(seat.picture(), Some((2560, 1440)));

        // The packing is two halves of one word, and a size that did not fit
        // would arrive as a different picture rather than as a refusal.
        shared.publish_picture(1920, 1080);
        assert_eq!(seat.picture(), Some((1920, 1080)));
    }

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

    /// **A seat that has not declared yet is not a vote against everything.**
    /// A guest takes its seat the moment it is streamable and its declaration
    /// reaches the seat a pass later; counting that gap as "can decode
    /// nothing" drags the stream to the base codec and straight back, which is
    /// two encoder rebuilds and two keyframes for nothing.
    #[test]
    fn a_seat_that_has_not_declared_does_not_vote() {
        const HEVC: u32 = lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC;

        let (shared, _stream, _arrivals) = parked();
        let active = [seat_of(0), seat_of(1)];

        shared.seats[0].flags.store(HEVC, Ordering::Relaxed);
        shared.seats[1].flags.store(0, Ordering::Relaxed);
        assert_eq!(
            consensus(&shared, &active),
            HEVC,
            "a seat mid-join was counted as decoding nothing"
        );

        // And once it does declare, it counts.
        shared.seats[1]
            .flags
            .store(lowlat_core::init::FLAG_BASE, Ordering::Relaxed);
        assert_eq!(consensus(&shared, &active), lowlat_core::init::FLAG_BASE);
    }

    /// A seat carries its occupant's declaration and not the last one's.
    #[test]
    fn a_claimed_seat_declares_nothing_until_its_guest_does() {
        let (shared, stream, _arrivals) = parked();
        let held = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        held.declare(lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC);
        held.request_reconfigure();
        drop(held);

        // The loop frees a leaving seat; do that part by hand.
        shared.seats[0]
            .state
            .store(seat_state::FREE, Ordering::Release);
        let next = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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

    /// Promote a taken seat without running the loop, which is what admits one
    /// in a session.
    fn streaming(shared: &Shared, index: usize) {
        shared.seats[index]
            .state
            .store(seat_state::STREAMING, Ordering::Release);
    }

    /// **One packet, one slot, and only the guests that asked for that
    /// encoding.** A room holding both kinds of guest is the case that cannot
    /// be checked by watching one of them.
    #[test]
    fn sound_reaches_only_the_seats_that_asked_for_that_encoding() {
        let (shared, stream, _arrivals) = parked();
        let compressed = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        let uncompressed = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        streaming(&shared, 0);
        streaming(&shared, 1);
        // The uncompressed form is a permission as well as a request.
        shared.set_sound(true, true, 128);
        compressed.declare_audio(false);
        uncompressed.declare_audio(true);
        assert_eq!(shared.audio_wanted(), (true, true));

        shared.publish_audio(false, b"compressed");
        let taken = compressed
            .next_audio()
            .expect("the compressed guest heard nothing");
        assert_eq!(taken.bytes(), b"compressed");
        drop(taken);
        assert!(
            uncompressed.next_audio().is_none(),
            "a guest was sent an encoding it did not ask for"
        );

        shared.publish_audio(true, b"uncompressed");
        let taken = uncompressed
            .next_audio()
            .expect("the uncompressed guest heard nothing");
        assert_eq!(taken.bytes(), b"uncompressed");
        drop(taken);
        assert!(compressed.next_audio().is_none());
    }

    /// **A guest asks and a host permits, and the header follows the host.** A
    /// peer that asked for the uncompressed form and was not granted it is sent
    /// the compressed one, and everything that produces, prices or labels a
    /// packet has to agree about which -- a guest sent one and told the other
    /// hears noise.
    #[test]
    fn a_guest_denied_the_uncompressed_form_is_sent_the_compressed_one() {
        let (shared, stream, _arrivals) = parked();
        let hold = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        streaming(&shared, 0);
        hold.declare_audio(true);

        shared.set_sound(true, false, 128);
        assert!(!hold.audio_raw(), "the permission was not consulted");
        assert_eq!(shared.audio_wanted(), (true, false));
        shared.publish_audio(false, b"compressed");
        assert!(hold.next_audio().is_some(), "nothing was sent");

        // Granted, the same guest gets what it asked for.
        shared.set_sound(true, true, 128);
        assert!(hold.audio_raw());
        assert_eq!(shared.audio_wanted(), (false, true));
        shared.publish_audio(true, b"uncompressed");
        let taken = hold.next_audio().expect("nothing was sent");
        assert_eq!(taken.bytes(), b"uncompressed");
    }

    /// **What sound costs follows the permission too**, or a host would price a
    /// guest at ten times what it is actually sending it.
    #[test]
    fn the_price_follows_what_is_actually_sent() {
        let (shared, stream, _arrivals) = parked();
        let hold = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        streaming(&shared, 0);
        hold.declare_audio(true);

        shared.set_sound(true, false, 128);
        let denied = shared.audio_mbps(128);
        shared.set_sound(true, true, 128);
        let granted = shared.audio_mbps(128);
        assert!(
            granted > denied * 5.0,
            "denied {denied} and granted {granted} were priced the same"
        );
    }

    /// **A room produces only what somebody is listening for**, which is what
    /// keeps the codec off a machine where every guest asked for the
    /// uncompressed form, and both off a machine with nobody in it.
    #[test]
    fn an_empty_room_wants_neither_encoding() {
        let (shared, stream, _arrivals) = parked();
        assert_eq!(shared.audio_wanted(), (false, false));
        let hold = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        // A seat that is claimed but not yet admitted is not listening either.
        assert_eq!(shared.audio_wanted(), (false, false));
        streaming(&shared, 0);
        shared.set_sound(true, true, 128);
        hold.declare_audio(false);
        assert_eq!(shared.audio_wanted(), (true, false));
        hold.declare_audio(true);
        assert_eq!(shared.audio_wanted(), (false, true));
    }

    /// **A room that empties gives the sound device back.**
    ///
    /// This is the one the live run caught: the device was taken by the loop
    /// that waits for a guest and given back by the same loop, which is never
    /// reached again while the encode loop is running -- so a host held a
    /// capture, and somebody's muted speakers, across three whole sessions.
    /// The decision now lives where the room's size is known, and it is
    /// recorded whether or not a device is configured, which is what lets this
    /// run without one.
    #[test]
    fn the_sound_device_is_given_back_when_the_room_empties() {
        let (shared, stream, arrivals) = parked();
        let hold = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        hold.declare(lowlat_core::init::FLAG_BASE);
        shared.set_sound(true, false, 128);

        let mut roster = Roster::default();
        let config = test_config(Codec::H264);
        // One pass to seat the guest, stopped as soon as it is streaming.
        let ticker = watcher(&shared, |shared| {
            shared.sound_demand.load(Ordering::Acquire) != 0
        });
        encode_loop(
            &shared,
            &arrivals,
            config.clone(),
            &mut roster,
            &mut fake_encoder(),
            None,
        );
        assert!(
            ticker.join().expect("ticker"),
            "the room never wanted sound"
        );
        assert_eq!(shared.sound_demand.load(Ordering::Acquire), 1);
        let asked = shared.sound_epoch.load(Ordering::Acquire);
        shared.stopping.store(0, Ordering::Release);

        // The guest leaves. **Nothing else happens**: no rebuild, no arrival,
        // no error -- which is exactly the case that was missed.
        drop(hold);
        let ticker = watcher(&shared, |shared| {
            shared.sound_demand.load(Ordering::Acquire) == 0
        });
        encode_loop(
            &shared,
            &arrivals,
            config,
            &mut roster,
            &mut fake_encoder(),
            None,
        );
        assert!(
            ticker.join().expect("ticker"),
            "the device was still held with nobody listening"
        );
        assert!(
            shared.sound_epoch.load(Ordering::Acquire) > asked,
            "the decision never changed"
        );
    }

    /// **A capture that stops is taken again.** The test above is that the
    /// pass which knows the room's size is the only thing that can give a
    /// device back; this is its other half, one level down. A capture ends on
    /// its own thread when the sound server goes away and tells nobody, so a
    /// host that had opened one once held a thread that had already returned,
    /// with the room still saying somebody was listening.
    ///
    /// Off by default: it needs a sound server, and it reaches that server
    /// through a proxy it can cut, which is how a server going away is
    /// produced without disturbing the one the machine is using.
    #[test]
    #[ignore = "needs a sound server and socat"]
    fn a_capture_that_stops_is_taken_again() {
        let socket = std::env::temp_dir().join(format!("lowlat-sound-{}", std::process::id()));
        let mut proxy = spawn_proxy(&socket).expect("a proxy to the sound server");
        let (shared, _stream, _arrivals) = parked();
        shared.set_sound(true, false, 128);
        let mut config = test_config(Codec::H264);
        config.audio = Some(lowlat_audio::Config {
            server: Some(format!("unix:{}", socket.display())),
            wanted: Arc::new(lowlat_audio::Wanted::default()),
        });

        reconcile_sound(&shared, 1, &config);
        assert!(reading(&shared), "the device was never taken");

        // The server goes away under a running capture.
        proxy.cut();
        assert!(
            settles(&shared, false, 5_000.0),
            "the capture never noticed that it had ended"
        );

        // **And a later pass takes it again, with nothing else happening**: no
        // arrival, no departure, no rebuild, and nobody asking.
        proxy = spawn_proxy(&socket).expect("a proxy to the sound server");
        assert!(
            retaken(&shared, &config, 15_000.0),
            "the device was never taken again"
        );

        reconcile_sound(&shared, 0, &config);
        drop(proxy);
        let _ = std::fs::remove_file(&socket);
    }

    /// **Having a sound source and being switched on are two things.** They
    /// were one, and the one they were was the source: a host that arrived
    /// with sound off had no source built, so the setter could turn the
    /// setting on and nothing would ever take a device -- the single field of
    /// that configuration that was not live, in the one structure documented
    /// as having no settled half.
    #[test]
    fn a_source_is_not_the_same_as_being_switched_on() {
        let mut config = test_config(Codec::H264);
        config.audio = Some(lowlat_audio::Config::default());

        config.audio_on = false;
        assert_eq!(
            SoundCells::new(&config).on.load(Ordering::Relaxed),
            0,
            "a host that arrived switched off started reading"
        );
        config.audio_on = true;
        assert_eq!(SoundCells::new(&config).on.load(Ordering::Relaxed), 1);

        // And a host with no source at all cannot be switched on, which is
        // what the daemon's own flag asks for.
        config.audio = None;
        assert_eq!(
            SoundCells::new(&config).on.load(Ordering::Relaxed),
            0,
            "sound was on with nothing to read"
        );
    }

    /// The same rule end to end: **switched on after starting off really takes
    /// the device**, and switched off gives it back.
    #[test]
    #[ignore = "needs a sound server"]
    fn sound_switched_on_after_starting_off_takes_the_device() {
        let (shared, _stream, _arrivals) = parked();
        let mut config = test_config(Codec::H264);
        config.audio = Some(lowlat_audio::Config::default());
        config.audio_on = false;
        // What the boundary sets when a host starts.
        shared.set_sound(config.audio_on, false, 128);

        reconcile_sound(&shared, 1, &config);
        assert!(
            !reading(&shared),
            "sound was taken while it was switched off"
        );

        shared.set_sound(true, false, 128);
        reconcile_sound(&shared, 1, &config);
        assert!(
            reading(&shared),
            "sound was never taken after being switched on"
        );

        shared.set_sound(false, false, 128);
        reconcile_sound(&shared, 1, &config);
        assert!(!reading(&shared), "the device was not given back");
    }

    /// A proxy that is cut however the test leaves.
    ///
    /// **Including through a failed assertion**, which otherwise leaves a
    /// listener behind holding the harness's own output open: the test reports
    /// nothing until somebody kills it by hand.
    struct Proxy(std::process::Child);

    impl Proxy {
        fn cut(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    impl Drop for Proxy {
        fn drop(&mut self) {
            self.cut();
        }
    }

    /// Whether the host is reading the sound device right now.
    fn reading(shared: &Shared) -> bool {
        shared
            .held_sound
            .lock()
            .is_ok_and(|held| held.as_ref().is_some_and(crate::audio::Sound::alive))
    }

    /// Wait for the device to be held, or not held, without a pass of the loop.
    fn settles(shared: &Shared, want: bool, within_ms: f64) -> bool {
        let began = lowlat_common::clock::Time::now();
        while lowlat_common::clock::elapsed_ms(began) < within_ms {
            if reading(shared) == want {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    /// The same, running the pass that is allowed to take it again.
    fn retaken(shared: &Arc<Shared>, config: &Config, within_ms: f64) -> bool {
        let began = lowlat_common::clock::Time::now();
        while lowlat_common::clock::elapsed_ms(began) < within_ms {
            reconcile_sound(shared, 1, config);
            if reading(shared) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    /// A path to the session's sound server that this test may cut.
    fn spawn_proxy(socket: &std::path::Path) -> Option<Proxy> {
        let runtime = std::env::var("XDG_RUNTIME_DIR").ok()?;
        let native = std::path::Path::new(&runtime).join("pulse").join("native");
        let _ = std::fs::remove_file(socket);
        // **One connection, served in this process.** A forking proxy leaves
        // the child holding the live connection when the listener is killed,
        // so the server would still be there after this test thought it had
        // taken it away.
        let child = std::process::Command::new("socat")
            .arg(format!("UNIX-LISTEN:{}", socket.display()))
            .arg(format!("UNIX-CONNECT:{}", native.display()))
            .spawn()
            .ok()?;
        // **The path exists before it is connectable and not before it is
        // bound**, so a connect that arrives first is refused rather than
        // queued.
        let began = lowlat_common::clock::Time::now();
        while !socket.exists() && lowlat_common::clock::elapsed_ms(began) < 2_000.0 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Some(Proxy(child))
    }

    /// **A guest behind on sound loses packets and the room does not.** The
    /// pool is shared, so a guest that stopped draining would otherwise hold
    /// every slot and silence everybody.
    #[test]
    fn a_guest_that_stops_listening_loses_packets_rather_than_the_room() {
        let (shared, stream, _arrivals) = parked();
        let behind = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        let listening = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        streaming(&shared, 0);
        streaming(&shared, 1);
        shared.set_sound(true, true, 128);
        behind.declare_audio(false);
        listening.declare_audio(false);

        // Far more than either ring holds, with one guest never draining.
        for _ in 0..AUDIO_DEPTH * 4 {
            shared.publish_audio(false, b"packet");
            let taken = listening
                .next_audio()
                .expect("the listening guest fell behind");
            assert_eq!(taken.bytes(), b"packet");
        }
        let mut held = 0usize;
        while let Some(packet) = behind.next_audio() {
            assert_eq!(packet.bytes(), b"packet");
            held += 1;
        }
        assert_eq!(
            held, AUDIO_DEPTH,
            "the ring grew or lost more than it holds"
        );
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
        let first = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        let second = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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
            config.clone(),
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
        let held = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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
        let held = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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
        let held = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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
        let held = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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
            let held = stream
                .seats()
                .take(wake_handle(), wake_handle())
                .expect("a seat");
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
        let first = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        let second = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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
        let asker = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
        let watcher = stream
            .seats()
            .take(wake_handle(), wake_handle())
            .expect("a seat");
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

    /// **Every reason a duplicate is still owed to somebody.**
    ///
    /// The failure this guards is a screen that stops updating, which no other
    /// test looks for and which a person notices before any measurement does.
    /// Each arm is here because dropping it breaks something specific, and the
    /// heartbeat is here because this list is written by hand and may be
    /// incomplete.
    #[test]
    fn a_duplicate_is_still_sent_when_somebody_needs_it() {
        // The whole point: an unchanged picture nobody is waiting for.
        assert!(
            !must_send(false, false, false, 0.0),
            "an unchanged picture was sent with nothing owed, so nothing is ever suppressed"
        );

        // A changed picture always goes, whatever else is true.
        assert!(
            must_send(true, false, false, 0.0),
            "a changed picture was held"
        );

        // A refresh is owed: a guest has fallen out of the reference chain or
        // has just joined, and a still screen is exactly when it would wait
        // forever.
        assert!(
            must_send(false, true, false, 0.0),
            "a guest waiting for a keyframe was made to wait for the screen to move"
        );

        // The seats moved: an arrival has received nothing at all.
        assert!(
            must_send(false, false, true, 0.0),
            "a guest that just took a seat was sent nothing because the screen was still"
        );

        // The heartbeat bounds how long any mistake above can freeze a screen.
        assert!(
            !must_send(false, false, false, HEARTBEAT_MS - 1.0),
            "the heartbeat fired early, which costs a picture a second for nothing"
        );
        assert!(
            must_send(false, false, false, HEARTBEAT_MS),
            "the heartbeat did not fire on its own boundary"
        );
        assert!(
            must_send(false, false, false, HEARTBEAT_MS * 10.0),
            "a screen suppressed for ten heartbeats stayed suppressed"
        );
    }

    /// **A finished picture is collected within a poll, not within a frame.**
    ///
    /// The loop only polls the encoder while it holds something, and the flag
    /// that says so has to be read after the submit rather than before it.
    /// Read before, the pass that submits a picture concludes there is nothing
    /// to collect and sleeps to the next frame tick, so every picture sits
    /// finished for a whole interval and the latency reported to a peer grows
    /// by one frame.
    ///
    /// **Pacing does not show this and neither does the frame rate.** Both stay
    /// exactly right while every picture is a frame late, which is how it
    /// reached a live session: the stage to watch is `encode`, not `interval`.
    /// The fake encoder answers immediately, so anything approaching an
    /// interval here is waiting rather than encoding.
    #[test]
    fn a_finished_picture_does_not_wait_for_the_next_frame() {
        let harness = Harness::start();
        let seat = harness.seat();

        // The report is published on a frame count, so the run has to reach it.
        let mut received = 0usize;
        until_within(30_000.0, "the timing report", || {
            while seat.next_frame().is_some() {
                received += 1;
            }
            received > REPORT_FRAMES as usize && harness.stream.timings().encode.p50 > 0.0
        });

        let report = harness.stream.timings();
        let interval = 1000.0 / 240.0;
        println!(
            "encode p50 {:.3} ms p99 {:.3} ms against a {interval:.3} ms interval",
            report.encode.p50, report.encode.p99
        );
        // **The tail is where this lives, not the median.** Only the pass that
        // submits is affected, so half the picture is collected normally and
        // the median barely moves: measured, the median was 1.08 ms either way
        // while the tail went from 1.07 to 16.6. Watching the median is how it
        // shipped.
        assert!(
            report.encode.p99 < interval,
            "a picture that was ready immediately took {:.3} ms at the tail, which is a whole \
             {interval:.3} ms frame interval -- it waited for the frame clock rather than the \
             poll (median {:.3} ms, which is why the median is not what is asserted)",
            report.encode.p99,
            report.encode.p50
        );
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
            audio: None,
            audio_on: false,
            accept_microphone: false,
            audio_kbps: 128,
            allow_raw_audio: false,
            output: None,
            display: false,
            width: 1920,
            height: 1080,
            fps: 60,
            cg_level: 1,
            full_fps: false,
            codec: Codec::H264,
            backend: Some(Backend::Open),
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
        });
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(
                wake.handle().expect("handle"),
                wake.handle().expect("a second handle"),
            )
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
            audio: None,
            audio_on: false,
            accept_microphone: false,
            audio_kbps: 128,
            allow_raw_audio: false,
            output: None,
            display: false,
            width: 1920,
            height: 1080,
            fps: 60,
            cg_level: 1,
            full_fps: false,
            codec: Codec::H264,
            backend: Some(Backend::Open),
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: rows,
        });
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(
                wake.handle().expect("handle"),
                wake.handle().expect("a second handle"),
            )
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

    /// **A refresh is counted where it was asked for, and only there.**
    ///
    /// The counter exists to say whether a stream is spending its rate on
    /// recovery, so the thing that has to be shown is that it moves under back
    /// pressure and stays still without it. A seat nobody drains fills its
    /// publish ring, the ring refuses, every guest behind it latches, and the
    /// gate grants a refresh: that path must land on `no_room` or `no_slot`.
    /// A seat drained promptly must not.
    #[test]
    fn a_refresh_is_counted_where_it_was_asked_for() {
        // The control first. Frames are taken as fast as they are made, so
        // nothing is ever behind and nothing asks for a recovery.
        let calm = {
            let stream = report_stream();
            let wake = lowlat_net::Wake::new().expect("wake");
            let seat = stream
                .seats()
                .take(
                    wake.handle().expect("handle"),
                    wake.handle().expect("a second handle"),
                )
                .expect("a free seat");
            let mut received = 0usize;
            let mut keyframes = 0usize;
            until_within(30_000.0, "the calm window to report", || {
                while let Some(frame) = seat.next_frame() {
                    received += 1;
                    if frame.keyframe() {
                        keyframes += 1;
                    }
                }
                received >= REPORT_FRAMES as usize + 60
            });
            let counted = stream.shared.refreshes.read();
            println!("calm: {received} frames, {keyframes} keyframes on the wire, {counted:?}");
            // **The wire is the independent witness.** The count is taken
            // inside the loop and the keyframes are what left it, so a counter
            // wired to the wrong thing disagrees with them.
            assert!(
                u64::from(counted.sent) <= keyframes as u64,
                "more refreshes counted than keyframes reached the wire: {counted:?} against \
                 {keyframes}"
            );
            counted
        };
        assert_eq!(
            calm.reinit, 0,
            "a run that reconfigured nothing counted a reinitialization refresh"
        );
        assert_eq!(
            calm.too_large, 0,
            "a frame that fitted its slot counted as one that did not"
        );

        // Now the pressure. The seat is taken and never read, so the publish
        // ring fills, refuses, and every frame after that is one its guest
        // missed.
        let squeezed = {
            let stream = report_stream();
            let wake = lowlat_net::Wake::new().expect("wake");
            let _seat = stream
                .seats()
                .take(
                    wake.handle().expect("handle"),
                    wake.handle().expect("a second handle"),
                )
                .expect("a free seat");
            until_within(30_000.0, "the squeezed window to report", || {
                stream
                    .shared
                    .refreshes
                    .read()
                    .counts()
                    .iter()
                    .any(|n| *n > 0)
            });
            let counted = stream.shared.refreshes.read();
            println!("squeezed: {counted:?}");
            counted
        };

        assert!(
            squeezed.no_room > 0 || squeezed.no_slot > 0,
            "a guest that never took a frame produced no refresh: {squeezed:?}"
        );
        assert!(
            squeezed.sent > 0,
            "refreshes were granted and none reached the encoder: {squeezed:?}"
        );
        assert_eq!(
            squeezed.reinit, 0,
            "back pressure was attributed to a reinitialization: {squeezed:?}"
        );
        assert!(
            squeezed.sent > calm.sent,
            "back pressure asked for no more refreshes than an idle stream: {squeezed:?} against \
             {calm:?}"
        );
    }

    /// A stream shaped for the refresh test: real pipeline, no display, and a
    /// rate low enough that a guest which never drains runs out of room.
    fn report_stream() -> Stream {
        Stream::start(Config {
            audio: None,
            audio_on: false,
            accept_microphone: false,
            audio_kbps: 128,
            allow_raw_audio: false,
            output: None,
            display: false,
            width: 1920,
            height: 1080,
            fps: 60,
            codec: Codec::H264,
            backend: Some(Backend::Open),
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
            full_fps: false,
            cg_level: 1,
        })
    }

    /// Run the real pipeline at `fps` until it has reported, and print the
    /// table docs/05-host.md section 10 asks for.
    fn measure(fps: u32, frames: usize) -> Report {
        let stream = Stream::start(Config {
            audio: None,
            audio_on: false,
            accept_microphone: false,
            audio_kbps: 128,
            allow_raw_audio: false,
            output: None,
            display: false,
            width: 1920,
            height: 1080,
            fps,
            codec: Codec::H264,
            backend: Some(Backend::Open),
            configured_mbps: 10.0,
            min_mbps: 1.0,
            rotation: lowlat_core::video::Rotation::None,
            detail_rows: 0,
            full_fps: false,
            cg_level: 1,
        });
        let wake = lowlat_net::Wake::new().expect("wake");
        let seat = stream
            .seats()
            .take(
                wake.handle().expect("handle"),
                wake.handle().expect("a second handle"),
            )
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
