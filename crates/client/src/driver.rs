//! The session, driven one pass at a time: what a client says when the path
//! comes up, what it does with each channel, and how far behind its reader is.
//!
//! Sans-IO. The shell thread calls [`Driver::turn`] once per pass with the
//! endpoint and the clock; a test calls it with a simulated endpoint and a
//! fake clock. Nothing here reads time, owns a socket or spawns a thread.
//!
//! **The receive ring is this thread's, so the catch-up is too.** Access
//! units are taken off the video channel here and handed to the decoder's
//! thread through a pool by index; when the reader is more than one message
//! behind, this looks ahead through the messages the ring holds for keyframe
//! metadata whose picture has also arrived and skips to it, so a lagging
//! reader recovers in one step. Nothing is skipped over a gap, and nothing is
//! skipped when no keyframe is ahead (docs/10-client.md section 3).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lowlat_common::events;
use lowlat_common::pool::{self, Pool};
use lowlat_common::spsc::Ring;
use lowlat_core::audio;
use lowlat_core::channel::Arrivals;
use lowlat_core::control::{self, CONTROL_CHANNEL, Control, op};
use lowlat_core::cursor;
use lowlat_core::endpoint::Endpoint;
use lowlat_core::init::{self, Init};
use lowlat_core::relay::{self, Failure, Relay};
use lowlat_core::session::{Health, Session};
use lowlat_core::video::{self, METADATA_LEN, VIDEO_HEADER_LEN};
use lowlat_core::{Error, conn};

use crate::cursor::{Cache, Shape};
use crate::input::{Input, Mapper, Viewport};
use crate::seam::{Event, Outcome};
use crate::sound::{self, Packets};
use crate::{AUDIO_CHANNEL, UNIT_BYTES, UNIT_SLOTS, VIDEO_CHANNEL};
use lowlat_core::pad;

/// The longest inbound control message that will be taken: the user-data
/// ceiling plus its header. A longer one cannot be consumed and the channel
/// cannot advance past it, so it ends the session.
const MAX_INBOUND: usize = control::USER_DATA_MAX + control::CONTROL_HEADER_LEN;

/// The streams a peer holds. Only the first is ever sent pictures.
const STREAMS: usize = 3;

/// The pool's tag on a unit that is keyframe metadata rather than a picture.
pub const TAG_METADATA: u32 = 1;

/// How often the client reports its decode times: on the clock, not per
/// picture, so a still desktop still reports and the round-trip estimate,
/// which samples only on acknowledged sends, stays alive.
pub const REPORT_INTERVAL_MS: f64 = 2000.0;

/// The media kinds of a latency report.
const KIND_VIDEO: u32 = 1;
const KIND_AUDIO: u32 = 2;

/// The channels the metrics describe: control, video, sound, by number.
pub const CHANNELS: usize = 3;

/// How often the recent-loss figure takes a sample, and the weight of each
/// sample in it: a thirtieth, so the figure reads over about thirty seconds.
const LOSS_SAMPLE_MS: f64 = 1000.0;
const LOSS_WEIGHT: f64 = 1.0 / 30.0;

/// Received access units, on their way to the decoder.
///
/// One producer (the session's thread) and one consumer. The pool holds the
/// bytes and the ring carries slot indices, so a unit is copied once, off the
/// receive ring, and never again.
#[derive(Debug, Clone)]
pub struct Units {
    pool: Arc<Pool>,
    ring: Arc<Ring<u32, UNIT_SLOTS>>,
    /// The consumer's wait word: bumped on every unit handed over.
    word: Arc<AtomicU32>,
}

impl Default for Units {
    fn default() -> Self {
        Self::new()
    }
}

impl Units {
    pub fn new() -> Self {
        Self {
            pool: Arc::new(Pool::new(UNIT_SLOTS, UNIT_BYTES)),
            ring: Arc::new(Ring::new()),
            word: Arc::new(AtomicU32::new(0)),
        }
    }

    /// Wait up to `timeout` for a unit to be handed over, or for a wake.
    /// The consumer's; it rechecks `take` after.
    pub fn wait(&self, timeout: core::time::Duration) {
        let word = self.word.load(Ordering::Acquire);
        if self.ring.is_empty() {
            lowlat_common::wait::wait(&self.word, word, timeout);
        }
    }

    /// Wake the consumer without a unit: teardown, or something else to
    /// look at.
    pub fn wake(&self) {
        self.word.fetch_add(1, Ordering::Release);
        lowlat_common::wait::notify_all(&self.word);
    }

    /// The next unit, in order, or `None` while nothing waits. Held until
    /// dropped; the slot goes back then.
    pub fn take(&self) -> Option<Unit<'_>> {
        let index = self.ring.pop()?;
        let frame = self.pool.claim(index)?;
        Some(Unit { frame })
    }

    /// Units handed over and not yet taken.
    pub fn queued(&self) -> usize {
        self.ring.len()
    }
}

/// One access unit, header included, as it came off the wire.
#[derive(Debug)]
pub struct Unit<'a> {
    frame: pool::Frame<'a>,
}

impl Unit<'_> {
    /// The message content: the ten-byte video header and the bitstream.
    pub fn bytes(&self) -> &[u8] {
        self.frame.bytes()
    }

    /// Keyframe metadata rather than a picture.
    pub fn metadata(&self) -> bool {
        self.frame.tag() & TAG_METADATA != 0
    }
}

/// How far behind the reader is: what the deferred decisions are decided on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lag {
    /// Messages arrived and not yet consumed: waiting in the receive ring plus
    /// handed over and not yet taken.
    pub behind: u32,
    /// How long there has been something unconsumed, in milliseconds.
    pub behind_ms: u32,
}

/// What the loop publishes about itself, for the seam to read from any thread.
#[derive(Debug, Default)]
pub struct Telemetry {
    /// The seam's state word: 0 connecting, 1 established, 2 over.
    pub state: AtomicU32,
    /// The status the host's disconnect carried, or zero.
    pub disconnect: AtomicU32,
    pub pictures: AtomicU64,
    pub metadata: AtomicU64,
    pub skipped: AtomicU64,
    pub video_bytes: AtomicU64,
    pub audio_packets: AtomicU64,
    pub audio_bytes: AtomicU64,
    pub control_in: AtomicU64,
    pub control_out: AtomicU64,
    pub behind: AtomicU32,
    pub behind_ms: AtomicU32,
    pub rtt_ms: AtomicU32,
    /// Set by the decoder's consumer when its feed asked for a keyframe; the
    /// loop sends the request on its next pass and clears it.
    pub request: AtomicBool,
    /// The decoder: 0 none yet, 1 built, 2 failed for good.
    pub decoder: AtomicU32,
    /// The last picture's decode and read-back, in microseconds.
    pub decode_us: AtomicU32,
    pub readback_us: AtomicU32,
    /// The smoothed decode and hand-over per picture, and per sound packet,
    /// in microseconds: what the client reports to the host.
    pub decode_reported_us: AtomicU32,
    pub audio_reported_us: AtomicU32,
    /// The declaration: what the application asked, and what went out after
    /// the mask; and the stream as decoded, the layout's code or zero.
    pub asked_flags: AtomicU32,
    pub declared_flags: AtomicU32,
    pub stream_format: AtomicU32,
    /// Bumped by the loop when the declaration changed mid-session: the
    /// decode thread tears its decoder down and asks for a keyframe.
    pub reconfigure: AtomicU32,
    /// Bumped by the loop when the application chose another decoder
    /// mid-session, once the declaration has been restated: the decode
    /// thread takes the pending choice, moves to it, and asks for a keyframe
    /// once the new decoder can take one.
    pub switch: AtomicU32,
    /// Pictures decoded and handed to the queue.
    pub decoded: AtomicU64,
    /// Pictures published and not yet taken by the application.
    pub queue_depth: AtomicU32,
    /// The host's own encode time for the stream, as it last reported it,
    /// in microseconds.
    pub encode_us: AtomicU32,
    /// The codec the decoder was built for, on the wire's numbering: 0
    /// none yet, 1 the first codec, 2 the second.
    pub codec: AtomicU32,
    /// Input reports refused because the ring to the session thread was
    /// full, counted where they were dropped.
    pub input_dropped: AtomicU32,
    /// A pad's own reports: sent to the host, received from it, and received
    /// for a pad this client never sent as reports, which are dropped.
    pub pad_reports_sent: AtomicU32,
    pub pad_reports_received: AtomicU32,
    pub pad_reports_dropped: AtomicU32,
    /// Sound: packets decoded and handed over; dropped because the pool
    /// was full; refused by the decoder; the last hand-over's age; the codec
    /// the decoder was built for on the wire's numbering, 0 for none.
    pub audio_decoded: AtomicU64,
    pub audio_dropped: AtomicU32,
    pub audio_refused: AtomicU32,
    pub audio_age_ms: AtomicU32,
    pub audio_codec: AtomicU32,
    /// This client's number on the host's roster; zero until told.
    pub number: AtomicU32,
    /// The pointer: pictures delivered, names the cache did not hold, and
    /// pictures the boundary's reader refused.
    pub cursor_images: AtomicU32,
    pub cursor_misses: AtomicU32,
    pub cursor_refused: AtomicU32,
    /// Per channel, what a receiver can count (docs/10-client.md section 9):
    /// fragments accepted, those that arrived behind a later one, duplicates
    /// and out-of-window drops, the negatives sent, bytes and messages taken
    /// off the channel, and the recent-loss figure as the bits of an `f32`.
    pub fragments: [AtomicU64; CHANNELS],
    pub late: [AtomicU64; CHANNELS],
    pub duplicates: [AtomicU64; CHANNELS],
    pub out_of_window: [AtomicU64; CHANNELS],
    pub nacks_sent: [AtomicU64; CHANNELS],
    pub bytes: [AtomicU64; CHANNELS],
    pub messages: [AtomicU64; CHANNELS],
    pub loss_30s: [AtomicU32; CHANNELS],
    /// How long the session has been established, in milliseconds.
    pub connected_ms: AtomicU32,
    /// The relayed address a relay attempt offered, as [`pack_relayed`]
    /// writes it; zero until the relay has one, and for a direct attempt.
    pub relayed: AtomicU64,
    /// Whether the path goes through the relay.
    pub path_relayed: AtomicBool,
}

/// A relayed address in one word, so it is published without a lock: the
/// IPv4 address in the low thirty-two bits, the port above it, and bit 48 set
/// so that no address at all reads as zero. The relayed family is IPv4.
pub fn pack_relayed(addr: std::net::SocketAddrV4) -> u64 {
    u64::from(addr.ip().to_bits()) | (u64::from(addr.port()) << 32) | (1 << 48)
}

/// The address [`pack_relayed`] wrote, if it wrote one.
pub fn unpack_relayed(word: u64) -> Option<std::net::SocketAddrV4> {
    if word & (1 << 48) == 0 {
        return None;
    }
    let ip = u32::try_from(word & 0xFFFF_FFFF).ok()?;
    let port = u16::try_from((word >> 32) & 0xFFFF).ok()?;
    Some(std::net::SocketAddrV4::new(
        std::net::Ipv4Addr::from_bits(ip),
        port,
    ))
}

/// What a failed relay ends the session with.
fn relay_outcome(relay: &Relay<'_>) -> Option<Outcome> {
    match relay.state() {
        relay::State::Failed(Failure::Unreachable) => Some(Outcome::RelayUnreachable),
        relay::State::Failed(Failure::Refused) => Some(Outcome::RelayRefused),
        relay::State::Failed(_) => Some(Outcome::RelayLost),
        _ => None,
    }
}

/// One session's driver.
#[derive(Debug)]
pub struct Driver {
    init: Init,
    units: Units,
    emit: events::Sender<Event>,
    telemetry: Arc<Telemetry>,
    established: bool,
    stalled_said: bool,
    /// When the departure went out, if it has.
    leaving: Option<f64>,
    /// When the next latency report is due.
    report_due_ms: Option<f64>,
    inbound: Vec<u8>,
    /// Sound packets on their way to the application, and the scratch a
    /// packet is taken into when the pool has no room for it.
    packets: Packets,
    dropped_sound: Vec<u8>,
    /// What this peer has sent and been sent, one count per opcode.
    received: [u32; 256],
    sent: [u32; 256],
    /// The generation the host announced per stream, and its encode time.
    generation: [Option<u32>; STREAMS],
    encode_latency_us: [u32; STREAMS],
    /// This client's number on the host's roster, once told.
    number: Option<u32>,
    /// Since when the reader has had something unconsumed.
    behind_since: Option<f64>,
    lag: Lag,
    pictures: u64,
    metadata: u64,
    skipped: u64,
    video_bytes: u64,
    audio_packets: u64,
    audio_bytes: u64,
    audio_dropped: u32,
    first_audio_said: bool,
    /// The window-to-picture mapping the application's input goes through.
    mapper: Mapper,
    /// The picture's size and turn as last seen, so the mapper is told only
    /// on a change.
    picture: (u16, u16, video::Rotation),
    /// Whether the host has this client in relative mode.
    relative: bool,
    /// The pointer pictures the host may name again, and the count of
    /// names it did not resolve.
    cursor_cache: Cache,
    cursor_images: u32,
    cursor_misses: u32,
    /// The picture last delivered, by checksum: a host that names it again,
    /// or sends it again, does not have the application decode it again.
    cursor_delivered: u32,
    /// Bytes taken off the control channel; the other two channels' are
    /// counted where their messages are taken.
    control_bytes: u64,
    /// When the session was established, and the recent-loss sampler: when
    /// it last sampled, the arrivals it saw then, and the average per channel.
    established_ms: f64,
    loss_sampled_ms: f64,
    loss_seen: [Arrivals; CHANNELS],
    loss_ewma: [f64; CHANNELS],
}

impl Driver {
    pub fn new(
        init: Init,
        units: Units,
        packets: Packets,
        emit: events::Sender<Event>,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        Self {
            init,
            units,
            packets,
            emit,
            telemetry,
            established: false,
            stalled_said: false,
            leaving: None,
            report_due_ms: None,
            inbound: vec![0u8; MAX_INBOUND],
            dropped_sound: vec![0u8; sound::PACKET_BYTES],
            received: [0; 256],
            sent: [0; 256],
            generation: [None; STREAMS],
            encode_latency_us: [0; STREAMS],
            number: None,
            behind_since: None,
            lag: Lag::default(),
            pictures: 0,
            metadata: 0,
            skipped: 0,
            video_bytes: 0,
            audio_packets: 0,
            audio_bytes: 0,
            audio_dropped: 0,
            first_audio_said: false,
            mapper: Mapper::default(),
            picture: (0, 0, video::Rotation::Unknown),
            relative: false,
            cursor_cache: Cache::default(),
            cursor_images: 0,
            cursor_misses: 0,
            cursor_delivered: 0,
            control_bytes: 0,
            established_ms: 0.0,
            loss_sampled_ms: 0.0,
            loss_seen: [Arrivals::default(); CHANNELS],
            loss_ewma: [0.0; CHANNELS],
        }
    }

    /// Where the application drew the picture.
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.mapper.set_viewport(viewport);
    }

    /// One report from the application, onto the wire if the rules allow it.
    pub fn send_input(&mut self, session: &mut Session<'_>, input: &Input) {
        if !self.established {
            return;
        }
        if let Input::PadReport {
            pad,
            product,
            kind,
            transport,
            len,
            report,
        } = input
        {
            let report = report.get(..usize::from(*len)).unwrap_or(&[]);
            for wire in self
                .mapper
                .encode_report(*pad, *product, *kind, *transport, report)
                .iter()
                .flatten()
            {
                self.send_control(session, &wire.control());
            }
            self.telemetry
                .pad_reports_sent
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Some(wire) = self.mapper.encode(input) {
            self.send_control(session, &wire.control());
        }
    }

    /// The application changed its declaration: restate it to the host on
    /// the secondary streams, and have the decode thread tear its decoder
    /// down and ask for the keyframe on the first. Before the session is
    /// established the new flags simply go into the initialization.
    pub fn set_flags(&mut self, session: &mut Session<'_>, flags: u32) {
        if !self.restate(session, flags) {
            return;
        }
        self.telemetry.reconfigure.fetch_add(1, Ordering::Release);
        self.units.wake();
    }

    /// The application chose another decoder mid-session: the declaration
    /// restated where it changed, then the one word to the decode thread,
    /// which moves to the choice it was handed and asks for the keyframe
    /// once the new decoder can take one -- one request per switch, whether
    /// or not the declaration moved with it.
    pub fn switch_decoder(&mut self, session: &mut Session<'_>, flags: u32) {
        self.restate(session, flags);
        self.telemetry.switch.fetch_add(1, Ordering::Release);
        self.units.wake();
    }

    /// The new declaration to the secondary streams, when it changed and the
    /// session is established; stream 0's travels with the keyframe request.
    /// Whether it changed.
    fn restate(&mut self, session: &mut Session<'_>, flags: u32) -> bool {
        if flags == self.init.flags {
            return false;
        }
        self.init.flags = flags;
        if !self.established {
            return false;
        }
        for stream in [2, 1] {
            self.send_control(
                session,
                &Control {
                    a0: stream,
                    a1: flags,
                    a2: 0,
                    opcode: op::ENCODER_CONFIG,
                    body: &[],
                },
            );
        }
        true
    }

    /// Whether the host has put this client in relative mode.
    pub fn relative(&self) -> bool {
        self.relative
    }

    pub fn established(&self) -> bool {
        self.established
    }

    /// Messages received, one count per opcode.
    pub fn received(&self) -> &[u32; 256] {
        &self.received
    }

    /// Messages sent, one count per opcode.
    pub fn sent(&self) -> &[u32; 256] {
        &self.sent
    }

    pub fn lag(&self) -> Lag {
        self.lag
    }

    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    pub fn pictures(&self) -> u64 {
        self.pictures
    }

    pub fn audio_packets(&self) -> u64 {
        self.audio_packets
    }

    pub fn generation(&self, stream: usize) -> Option<u32> {
        self.generation.get(stream).copied().flatten()
    }

    pub fn encode_latency_us(&self, stream: usize) -> u32 {
        self.encode_latency_us.get(stream).copied().unwrap_or(0)
    }

    /// One pass. `Some` is the end of the session, and the last thing this
    /// driver decides.
    pub fn turn(
        &mut self,
        endpoint: &mut Endpoint<'_, Session<'_>>,
        now_ms: f64,
    ) -> Option<Outcome> {
        // The relay's end is its own, typed, and comes before the punch's:
        // an attempt whose relay failed can only time out after it.
        if let Some(outcome) = endpoint.relay().and_then(relay_outcome) {
            lowlat_common::log_info!("client: the relay ended the attempt, outcome={outcome:?}");
            return Some(outcome);
        }
        match endpoint.conn().state() {
            conn::State::Established(addr) if !self.established => {
                self.established = true;
                self.established_ms = now_ms;
                self.telemetry
                    .path_relayed
                    .store(endpoint.relay().is_some(), Ordering::Relaxed);
                self.loss_sampled_ms = now_ms;
                self.telemetry.state.store(1, Ordering::Relaxed);
                self.start(endpoint.session());
                self.report_due_ms = Some(now_ms + REPORT_INTERVAL_MS);
                self.emit.send(Event::Established { addr });
            }
            conn::State::Failed(failure) => {
                lowlat_common::log_info!("client: punch failed, outcome={failure:?}");
                return Some(Outcome::ConnectivityFailed);
            }
            _ => {}
        }
        if !self.established {
            return None;
        }

        if let Some(outcome) = self.check_health(endpoint, now_ms) {
            return Some(outcome);
        }
        if self.telemetry.request.swap(false, Ordering::AcqRel) {
            self.request_keyframe(endpoint.session());
        }
        if self.report_due_ms.is_some_and(|due| now_ms >= due) {
            self.report(endpoint.session());
            self.report_due_ms = Some(now_ms + REPORT_INTERVAL_MS);
        }
        if let Some(outcome) = self.drain_control(endpoint.session()) {
            return Some(outcome);
        }
        if let Some(outcome) = self.drain_video(endpoint.session()) {
            return Some(outcome);
        }
        if let Some(outcome) = self.drain_audio(endpoint.session(), now_ms) {
            return Some(outcome);
        }
        self.measure_lag(endpoint.session(), now_ms);
        self.sample_loss(endpoint.session(), now_ms);
        self.publish(endpoint.session(), now_ms);
        None
    }

    /// Once a second, fold each channel's late arrivals over its arrivals
    /// since the last sample into the recent-loss average.
    fn sample_loss(&mut self, session: &Session<'_>, now_ms: f64) {
        if now_ms - self.loss_sampled_ms < LOSS_SAMPLE_MS {
            return;
        }
        self.loss_sampled_ms = now_ms;
        for (channel, (seen, ewma)) in self
            .loss_seen
            .iter_mut()
            .zip(self.loss_ewma.iter_mut())
            .enumerate()
        {
            let now = session
                .recv_arrivals(u8::try_from(channel).unwrap_or(u8::MAX))
                .unwrap_or_default();
            let fragments = now.fragments.saturating_sub(seen.fragments);
            let late = now.late.saturating_sub(seen.late);
            *seen = now;
            // Nothing arrived, nothing was lost: a quiet channel reads as a
            // clean one rather than holding its last figure for ever.
            #[allow(clippy::cast_precision_loss, reason = "counts per second")]
            let sample = if fragments == 0 {
                0.0
            } else {
                (late as f64 / fragments as f64).min(1.0)
            };
            *ewma += (sample - *ewma) * LOSS_WEIGHT;
        }
    }

    /// The departure: opcode 10 with a zero status, which is what a client
    /// sends when it leaves cleanly. The session stays up for a grace so the
    /// message reaches the host; [`Driver::left`] says when that is over.
    pub fn leave(&mut self, session: &mut Session<'_>, now_ms: f64) {
        if self.leaving.is_none() {
            self.send_control(
                session,
                &Control {
                    a0: 0,
                    a1: 0,
                    a2: 0,
                    opcode: op::DISCONNECT,
                    body: &[],
                },
            );
            self.leaving = Some(now_ms);
        }
    }

    /// True once a departure has had its grace.
    pub fn left(&self, now_ms: f64) -> bool {
        self.leaving
            .is_some_and(|at| now_ms - at >= crate::seam::LEAVE_GRACE_MS)
    }

    /// An application message for the host.
    pub fn send_user_data(&mut self, session: &mut Session<'_>, id: u32, text: &[u8]) -> bool {
        let len = control::string_body_len(text.len());
        if len > control::USER_DATA_MAX {
            return false;
        }
        let mut header = [0u8; control::CONTROL_HEADER_LEN];
        let control = Control {
            a0: u32::try_from(len).unwrap_or(u32::MAX),
            a1: id,
            a2: 0,
            opcode: op::USER_DATA,
            body: &[],
        };
        if control::encode_header(&mut header, &control).is_err() {
            return false;
        }
        // The body is the text and its terminator, which the message framing
        // takes as two slices; the terminator rides as a one-byte tail.
        let mut body = Vec::with_capacity(len);
        body.extend_from_slice(text);
        body.push(0);
        let sent = session
            .send_message(CONTROL_CHANNEL, &header, &body)
            .is_ok();
        if sent {
            self.count_sent(op::USER_DATA);
        }
        sent
    }

    /// What a client says when the path comes up, in order: the
    /// initialization, the diagnostics opcode with every bit clear, and the
    /// declaration for each secondary stream. Stream 0 declares through the
    /// initialization and sends no declaration of its own; nothing waits on an
    /// acknowledgement.
    fn start(&mut self, session: &mut Session<'_>) {
        let mut body = [0u8; 512];
        let Ok(len) = init::encode(&mut body, &self.init) else {
            return;
        };
        let init = Control {
            a0: u32::try_from(len).unwrap_or(u32::MAX),
            a1: 0,
            a2: 0,
            opcode: op::INIT,
            body: body.get(..len).unwrap_or(&[]),
        };
        self.send_control(session, &init);
        self.send_control(
            session,
            &Control {
                a0: 0,
                a1: 0,
                a2: 0,
                opcode: op::DIAGNOSTICS,
                body: &[],
            },
        );
        for stream in [2, 1] {
            self.send_control(
                session,
                &Control {
                    a0: stream,
                    a1: self.init.flags,
                    a2: 0,
                    opcode: op::ENCODER_CONFIG,
                    body: &[],
                },
            );
        }
    }

    /// The one request a client makes of a host: a keyframe for stream 0,
    /// after its decoder was torn down. An encoder rebuild on an established
    /// host, so it is sent only when the feed decided it must be.
    fn request_keyframe(&mut self, session: &mut Session<'_>) {
        self.send_control(
            session,
            &Control {
                a0: 0,
                a1: self.init.flags,
                a2: 1,
                opcode: op::ENCODER_CONFIG,
                body: &[],
            },
        );
    }

    /// The decode times, both kinds, in the argument order a client uses:
    /// the figure first, then the kind. Zero until something has been timed,
    /// and sent anyway.
    fn report(&mut self, session: &mut Session<'_>) {
        let video = self.telemetry.decode_reported_us.load(Ordering::Relaxed);
        let audio = self.telemetry.audio_reported_us.load(Ordering::Relaxed);
        for (us, kind) in [(video, KIND_VIDEO), (audio, KIND_AUDIO)] {
            self.send_control(
                session,
                &Control {
                    a0: us,
                    a1: kind,
                    a2: 0,
                    opcode: op::ENCODE_LATENCY,
                    body: &[],
                },
            );
        }
    }

    fn send_control(&mut self, session: &mut Session<'_>, control: &Control<'_>) {
        let mut header = [0u8; control::CONTROL_HEADER_LEN];
        if control::encode_header(&mut header, control).is_err() {
            return;
        }
        if session
            .send_message(CONTROL_CHANNEL, &header, control.body)
            .is_ok()
        {
            self.count_sent(control.opcode);
        }
    }

    fn count_sent(&mut self, opcode: u8) {
        if let Some(count) = self.sent.get_mut(usize::from(opcode)) {
            *count = count.saturating_add(1);
        }
    }

    fn check_health(
        &mut self,
        endpoint: &Endpoint<'_, Session<'_>>,
        now_ms: f64,
    ) -> Option<Outcome> {
        match endpoint.health(now_ms) {
            Health::Dead => Some(Outcome::PeerGone),
            Health::Undeliverable => Some(Outcome::Undeliverable),
            Health::Stalled => {
                if !self.stalled_said {
                    self.stalled_said = true;
                    lowlat_common::log_warn!(
                        "client: stalled, nothing received for {:.0} ms",
                        lowlat_core::session::LIVENESS_SOFT_MS
                    );
                }
                None
            }
            Health::Alive => {
                self.stalled_said = false;
                None
            }
        }
    }

    /// Everything the host said on the control channel.
    ///
    /// **A take that fails is terminal.** The channel only advances when a
    /// message is consumed, so a message that cannot be would be read again
    /// on every pass for ever.
    fn drain_control(&mut self, session: &mut Session<'_>) -> Option<Outcome> {
        // Borrowed out for the pass so the handler below may take the rest
        // of the driver; the buffer is put back on every way out.
        let mut inbound = core::mem::take(&mut self.inbound);
        let outcome = self.drain_control_into(session, &mut inbound);
        self.inbound = inbound;
        outcome
    }

    fn drain_control_into(
        &mut self,
        session: &mut Session<'_>,
        inbound: &mut [u8],
    ) -> Option<Outcome> {
        loop {
            let len = match session.take_message(CONTROL_CHANNEL, inbound) {
                None => return None,
                Some(Ok(len)) => len,
                Some(Err(error)) => {
                    lowlat_common::log_warn!("client: control message refused, error={error:?}");
                    return Some(Outcome::Unreadable);
                }
            };
            self.control_bytes = self.control_bytes.saturating_add(len as u64);
            let Ok(message) = control::parse(inbound.get(..len).unwrap_or(&[])) else {
                continue;
            };
            if let Some(count) = self.received.get_mut(usize::from(message.opcode)) {
                if *count == 0 {
                    lowlat_common::log_info!(
                        "client: first op={} ({}) a0={} a1={} a2={} body={}",
                        message.opcode,
                        op::name(message.opcode),
                        message.a0,
                        message.a1,
                        message.a2,
                        message.body.len()
                    );
                }
                *count = count.saturating_add(1);
            }
            if let Some(outcome) = self.on_control(&message) {
                return Some(outcome);
            }
        }
    }

    /// The control vocabulary of docs/10-client.md section 7.
    fn on_control(&mut self, message: &Control<'_>) -> Option<Outcome> {
        #[allow(
            clippy::cast_possible_wrap,
            reason = "a status is signed and travels in an unsigned argument"
        )]
        match message.opcode {
            op::CURSOR => {
                if let Ok(pointer) = cursor::parse(message) {
                    self.on_cursor(&pointer);
                }
            }
            // Per-frame timing is not asked for and not read.
            op::FRAME_TIMING => {}
            // (pad, large, small): the motors travel as bytes in the low
            // eight bits of their arguments.
            op::RUMBLE => self.emit.send(Event::Rumble {
                pad: message.a0,
                large: (message.a1 & 0xFF) as u8,
                small: (message.a2 & 0xFF) as u8,
            }),
            // What the host's device was written, framed for the pad it
            // names; a pad never sent as reports has no framing and is
            // dropped.
            op::PAD_OUTPUT => {
                if let Some(output) = pad::parse_output(message) {
                    let mut framed = [0u8; pad::REPORT_MAX];
                    match self.mapper.frame_output(
                        output.pad,
                        output.kind,
                        output.report,
                        &mut framed,
                    ) {
                        Some(len) => {
                            self.telemetry
                                .pad_reports_received
                                .fetch_add(1, Ordering::Relaxed);
                            self.emit.send(Event::PadReport {
                                pad: output.pad,
                                kind: output.kind,
                                len: u8::try_from(len).unwrap_or(u8::MAX),
                                report: framed,
                            });
                        }
                        None => {
                            self.telemetry
                                .pad_reports_dropped
                                .fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
            op::DISCONNECT => {
                let status = message.a0 as i32;
                self.telemetry
                    .disconnect
                    .store(message.a0, Ordering::Relaxed);
                // **Zero is not an ending.** A peer stores the status and
                // stops on a non-zero one.
                if status != 0 {
                    return Some(Outcome::Disconnected(status));
                }
            }
            op::STREAM_ENDED => self.emit.send(Event::StreamEnded {
                stream: message.a0,
                status: message.a2 as i32,
            }),
            op::BLOCKED => self.emit.send(Event::Blocked {
                blocked: message.a0 != 0,
            }),
            op::USER_DATA => {
                if let Some((id, text)) = control::user_data(message) {
                    self.emit.send(Event::UserData {
                        id,
                        text: text.to_vec(),
                    });
                }
            }
            // A host sends (kind, microseconds, stream).
            op::ENCODE_LATENCY => {
                if let Some(slot) = self.encode_latency_us.get_mut(message.a2 as usize) {
                    *slot = message.a1;
                }
            }
            // The body is the host's application's; what the library reads
            // is its own number beside it.
            op::GUEST_LIST => {
                self.number = Some(message.a1);
                self.telemetry.number.store(message.a1, Ordering::Relaxed);
                let body = message.body.strip_suffix(&[0]).unwrap_or(message.body);
                self.emit.send(Event::GuestList {
                    number: message.a1,
                    body: body.to_vec(),
                });
            }
            op::HOST_MODE => self.emit.send(Event::HostMode { mode: message.a0 }),
            // (stream, generation, 0): the value the video header's frame
            // identifier will carry from the next encoder.
            op::ENCODER_GENERATION => {
                if let Some(slot) = self.generation.get_mut(message.a0 as usize) {
                    *slot = Some(message.a1);
                }
            }
            _ => {}
        }
        None
    }

    /// The host's pointer: the picture it sent or named, the mode it is in,
    /// and where it reappears.
    ///
    /// **Either bit puts a client into relative mode**, and that event is
    /// raised on the transition alone. The cursor event is raised on every
    /// update; a name the cache does not hold delivers it without a picture,
    /// and a name that carries no size takes the picture's size and hotspot
    /// from what was stored with it (docs/10-client.md section 7).
    fn on_cursor(&mut self, pointer: &cursor::Message<'_>) {
        // One stream: a pointer for another belongs to a picture this
        // client does not show.
        if pointer.update.stream != 0 {
            return;
        }
        if pointer.flags.contains(cursor::Flags::FORGET) {
            self.cursor_cache.clear();
        }
        let mut shape = Shape {
            width: pointer.width,
            height: pointer.height,
            hot_x: pointer.update.hot_x,
            hot_y: pointer.update.hot_y,
        };
        let mut checksum = 0;
        let mut png = Vec::new();
        if pointer.flags.contains(cursor::Flags::IMAGE) && !pointer.image.is_empty() {
            checksum = self.cursor_cache.insert(shape, pointer.image);
            png = pointer.image.to_vec();
        } else if pointer.flags.contains(cursor::Flags::CACHED) {
            match self.cursor_cache.get(pointer.checksum) {
                Some((stored, bytes)) => {
                    if pointer.width == 0 {
                        shape = stored;
                    }
                    checksum = pointer.checksum;
                    png = bytes.to_vec();
                }
                None => {
                    self.cursor_misses = self.cursor_misses.saturating_add(1);
                    self.telemetry
                        .cursor_misses
                        .store(self.cursor_misses, Ordering::Relaxed);
                    if self.cursor_misses == 1 {
                        lowlat_common::log_warn!(
                            "client: cursor named a picture not held, checksum={:#010x}",
                            pointer.checksum
                        );
                    }
                }
            }
        }
        // The same picture as last time travels as its name alone: the
        // application holds the picture it was given, and a host that
        // repeats a name on every update would otherwise have it decoded
        // on every update.
        if !png.is_empty() && checksum == self.cursor_delivered {
            png = Vec::new();
        }
        if !png.is_empty() {
            self.cursor_delivered = checksum;
            self.cursor_images = self.cursor_images.saturating_add(1);
            self.telemetry
                .cursor_images
                .store(self.cursor_images, Ordering::Relaxed);
        }

        let want = pointer.update.relative || pointer.update.hidden;
        let (x, y) = self.mapper.to_window(pointer.update.x, pointer.update.y);
        if want != self.relative {
            self.relative = want;
            self.emit.send(Event::Relative {
                relative: want,
                x,
                y,
            });
        }
        self.emit.send(Event::Cursor {
            x,
            y,
            width: shape.width,
            height: shape.height,
            hot_x: shape.hot_x,
            hot_y: shape.hot_y,
            hidden: pointer.update.hidden,
            relative: pointer.update.relative,
            suppressed: pointer.update.suppressed,
            checksum,
            png,
        });
    }

    /// Every complete access unit on the video channel, into the pool.
    ///
    /// Before each take, if the reader is more than one message behind, look
    /// ahead for an announced keyframe and skip to it. **The producer never
    /// blocks**: a full pool leaves the backlog in the receive ring, where the
    /// next pass's look-ahead sees it.
    fn drain_video(&mut self, session: &mut Session<'_>) -> Option<Outcome> {
        loop {
            let pending = session.pending_messages(VIDEO_CHANNEL);
            if pending == 0 {
                return None;
            }
            if pending > 1 {
                self.catch_up(session, pending);
            }
            let mut writer = self.units.pool.acquire()?;
            let mut refused: Option<Error> = None;
            let filled = writer.fill_with(|slot| match session.take_message(VIDEO_CHANNEL, slot) {
                Some(Ok(len)) => Some(len),
                Some(Err(error)) => {
                    refused = Some(error);
                    None
                }
                None => None,
            });
            if let Some(error) = refused {
                lowlat_common::log_warn!("client: video message refused, error={error:?}");
                return Some(Outcome::Unreadable);
            }
            if !filled {
                return None;
            }
            let content = writer.written();
            let len = content.len();
            let header = video::parse(content).ok();
            let is_metadata = header.is_some_and(|header| header.metadata);
            // The picture's size is the coordinate space the host expects
            // absolute input in, so the mapper follows the stream itself.
            if let Some(header) = header.filter(|header| !header.metadata) {
                let seen = (header.width, header.height, header.rotation);
                if seen != self.picture {
                    self.picture = seen;
                    self.mapper
                        .set_picture(header.width, header.height, header.rotation);
                }
            }
            let tag = if is_metadata { TAG_METADATA } else { 0 };
            // **The pool never refuses here.** The ring is as deep as the
            // pool has slots, so a slot that was free has a place in it.
            if writer.publish(tag, &[&self.units.ring]) == 0 {
                self.skipped = self.skipped.saturating_add(1);
                continue;
            }
            self.units.word.fetch_add(1, Ordering::Release);
            lowlat_common::wait::notify_one(&self.units.word);
            self.video_bytes = self.video_bytes.saturating_add(len as u64);
            if is_metadata {
                self.metadata = self.metadata.saturating_add(1);
            } else {
                self.pictures = self.pictures.saturating_add(1);
            }
        }
    }

    /// The keyframe-aligned catch-up over messages that have arrived.
    ///
    /// Reads the head of each pending message for keyframe metadata whose
    /// keyframe is the next message and has arrived whole, takes the last
    /// such, and discards everything before it. Nothing else is ever
    /// skipped: a reader behind by a backlog of predicted pictures decodes
    /// them in order, because none of them can be skipped to.
    fn catch_up(&mut self, session: &mut Session<'_>, pending: u32) {
        let mut head = [0u8; METADATA_LEN];
        let mut target: Option<u32> = None;
        let mut pictures_before = 0u32;
        let mut pictures_scanned = 0u32;
        for n in 0..pending {
            let Some(len) = session.peek_message(VIDEO_CHANNEL, n, &mut head) else {
                break;
            };
            let Ok(header) = video::parse(head.get(..len).unwrap_or(&[])) else {
                pictures_scanned = pictures_scanned.saturating_add(1);
                continue;
            };
            if !header.metadata {
                pictures_scanned = pictures_scanned.saturating_add(1);
                continue;
            }
            let announces_keyframe = video::parse_metadata(head.get(..len).unwrap_or(&[]))
                .is_ok_and(|metadata| metadata.keyframe);
            // The picture it announces must be the next message, complete,
            // and a picture: a second metadata message there is a keyframe
            // that has not arrived.
            let mut next = [0u8; VIDEO_HEADER_LEN];
            let next_is_picture = n + 1 < pending
                && session
                    .peek_message(VIDEO_CHANNEL, n + 1, &mut next)
                    .is_some_and(|len| {
                        video::parse(next.get(..len).unwrap_or(&[]))
                            .is_ok_and(|header| !header.metadata)
                    });
            if announces_keyframe && next_is_picture && n > 0 {
                target = Some(n);
                pictures_before = pictures_scanned;
            }
        }
        if let Some(n) = target {
            let skipped = session.skip_messages(VIDEO_CHANNEL, n);
            lowlat_common::log_info!(
                "client: keyframe {n} messages ahead, skipping to it, discarded={skipped}"
            );
            self.skipped = self.skipped.saturating_add(u64::from(pictures_before));
        }
    }

    /// Every sound packet, into the pool for the application, stamped with
    /// when it arrived.
    ///
    /// **A full pool drops the packet rather than leaving it.** Left in the
    /// receive ring it would sit behind everything the host sends after it,
    /// and a reader that is not calling would play it late when it did; the
    /// packet is taken off the ring and counted instead.
    fn drain_audio(&mut self, session: &mut Session<'_>, now_ms: f64) -> Option<Outcome> {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "milliseconds since the loop's epoch, wrapping as a stamp"
        )]
        let arrived = now_ms.max(0.0) as u32;
        loop {
            let mut refused: Option<Error> = None;
            let mut take = |slot: &mut [u8]| match session.take_message(AUDIO_CHANNEL, slot) {
                Some(Ok(len)) => Some(len),
                Some(Err(error)) => {
                    refused = Some(error);
                    None
                }
                None => None,
            };
            let len = if let Some(mut writer) = self.packets.writer() {
                if !writer.fill_with(&mut take) {
                    if let Some(error) = refused {
                        lowlat_common::log_warn!("client: sound packet refused, error={error:?}");
                        return Some(Outcome::Unreadable);
                    }
                    return None;
                }
                let len = writer.written().len();
                if !self.first_audio_said {
                    self.first_audio_said = true;
                    if let Ok(header) = audio::parse(writer.written()) {
                        lowlat_common::log_info!(
                            "client: sound mask={} samples={} rate={} codec={:?} channels={}",
                            header.mask,
                            header.samples,
                            header.rate,
                            header.codec,
                            header.channels
                        );
                    }
                }
                self.packets.publish(writer, arrived);
                len
            } else {
                let Some(len) = take(&mut self.dropped_sound) else {
                    if let Some(error) = refused {
                        lowlat_common::log_warn!("client: sound packet refused, error={error:?}");
                        return Some(Outcome::Unreadable);
                    }
                    return None;
                };
                self.audio_dropped = self.audio_dropped.saturating_add(1);
                if self.audio_dropped == 1 {
                    lowlat_common::log_warn!("client: sound pool full, packet dropped");
                }
                len
            };
            self.audio_packets = self.audio_packets.saturating_add(1);
            self.audio_bytes = self.audio_bytes.saturating_add(len as u64);
        }
    }

    /// How far behind the reader is: what waits in the ring plus what was
    /// handed over and not taken, and for how long there has been anything.
    fn measure_lag(&mut self, session: &Session<'_>, now_ms: f64) {
        let pending = session.pending_messages(VIDEO_CHANNEL);
        let queued = u32::try_from(self.units.queued()).unwrap_or(u32::MAX);
        let behind = pending.saturating_add(queued);
        let since = if behind == 0 {
            None
        } else {
            Some(self.behind_since.unwrap_or(now_ms))
        };
        self.behind_since = since;
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a non-negative duration in whole milliseconds, saturated"
        )]
        let behind_ms = since.map_or(0, |at| {
            (now_ms - at).max(0.0).min(f64::from(u32::MAX)) as u32
        });
        self.lag = Lag { behind, behind_ms };
    }

    fn publish(&self, session: &Session<'_>, now_ms: f64) {
        let t = &self.telemetry;
        // Per channel, what the ring and the session counted and what this
        // driver took off each.
        let messages = [
            self.received.iter().map(|c| u64::from(*c)).sum(),
            self.pictures.saturating_add(self.metadata),
            self.audio_packets,
        ];
        let bytes = [self.control_bytes, self.video_bytes, self.audio_bytes];
        for channel in 0..CHANNELS {
            let index = u8::try_from(channel).unwrap_or(u8::MAX);
            let arrivals = session.recv_arrivals(index).unwrap_or_default();
            let drops = session.recv_drops(index).unwrap_or_default();
            let store = |slot: &[AtomicU64; CHANNELS], value: u64| {
                if let Some(cell) = slot.get(channel) {
                    cell.store(value, Ordering::Relaxed);
                }
            };
            store(&t.fragments, arrivals.fragments);
            store(&t.late, arrivals.late);
            store(&t.duplicates, drops.duplicate);
            store(&t.out_of_window, drops.out_of_window);
            store(&t.nacks_sent, session.nacks_sent(index));
            store(&t.bytes, bytes.get(channel).copied().unwrap_or(0));
            store(&t.messages, messages.get(channel).copied().unwrap_or(0));
            #[allow(clippy::cast_possible_truncation, reason = "a ratio in 0..1")]
            if let (Some(cell), Some(ewma)) = (t.loss_30s.get(channel), self.loss_ewma.get(channel))
            {
                cell.store((*ewma as f32).to_bits(), Ordering::Relaxed);
            }
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "milliseconds since the session came up, saturated"
        )]
        t.connected_ms.store(
            (now_ms - self.established_ms)
                .max(0.0)
                .min(f64::from(u32::MAX)) as u32,
            Ordering::Relaxed,
        );
        t.pictures.store(self.pictures, Ordering::Relaxed);
        t.metadata.store(self.metadata, Ordering::Relaxed);
        t.skipped.store(self.skipped, Ordering::Relaxed);
        t.video_bytes.store(self.video_bytes, Ordering::Relaxed);
        t.audio_packets.store(self.audio_packets, Ordering::Relaxed);
        t.audio_bytes.store(self.audio_bytes, Ordering::Relaxed);
        t.audio_dropped.store(self.audio_dropped, Ordering::Relaxed);
        t.control_in.store(
            self.received.iter().map(|c| u64::from(*c)).sum(),
            Ordering::Relaxed,
        );
        t.control_out.store(
            self.sent.iter().map(|c| u64::from(*c)).sum(),
            Ordering::Relaxed,
        );
        t.behind.store(self.lag.behind, Ordering::Relaxed);
        t.behind_ms.store(self.lag.behind_ms, Ordering::Relaxed);
        t.encode_us
            .store(self.encode_latency_us(0), Ordering::Relaxed);
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a round trip in whole milliseconds, saturated"
        )]
        t.rtt_ms.store(
            session.srtt_ms().max(0.0).min(f64::from(u32::MAX)) as u32,
            Ordering::Relaxed,
        );
    }
}
