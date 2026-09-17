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
use lowlat_core::control::{self, CONTROL_CHANNEL, Control, op};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::init::{self, Init};
use lowlat_core::session::{Health, Session};
use lowlat_core::video::{self, METADATA_LEN, VIDEO_HEADER_LEN};
use lowlat_core::{Error, conn};

use crate::seam::{Event, Outcome};
use crate::{AUDIO_CHANNEL, UNIT_BYTES, UNIT_SLOTS, VIDEO_CHANNEL};

/// The longest inbound control message that will be taken: the user-data
/// ceiling plus its header. A longer one cannot be consumed and the channel
/// cannot advance past it, so it ends the session.
const MAX_INBOUND: usize = control::USER_DATA_MAX + control::CONTROL_HEADER_LEN;

/// The longest sound packet: the uncompressed ceiling plus its header.
const MAX_SOUND: usize = audio::PCM_PAYLOAD_MAX + audio::AUDIO_HEADER_LEN;

/// The streams a peer holds. Only the first is ever sent pictures.
const STREAMS: usize = 3;

/// The pool's tag on a unit that is keyframe metadata rather than a picture.
pub const TAG_METADATA: u32 = 1;

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
    /// Pictures decoded and handed to the queue.
    pub decoded: AtomicU64,
    /// Pictures published and not yet taken by the application.
    pub queue_depth: AtomicU32,
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
    inbound: Vec<u8>,
    sound: Vec<u8>,
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
    last_audio: Option<audio::AudioHeader>,
}

impl Driver {
    pub fn new(
        init: Init,
        units: Units,
        emit: events::Sender<Event>,
        telemetry: Arc<Telemetry>,
    ) -> Self {
        Self {
            init,
            units,
            emit,
            telemetry,
            established: false,
            stalled_said: false,
            leaving: None,
            inbound: vec![0u8; MAX_INBOUND],
            sound: vec![0u8; MAX_SOUND],
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
            last_audio: None,
        }
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
        match endpoint.conn().state() {
            conn::State::Established(addr) if !self.established => {
                self.established = true;
                self.telemetry.state.store(1, Ordering::Relaxed);
                self.start(endpoint.session());
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
        if let Some(outcome) = self.drain_control(endpoint.session()) {
            return Some(outcome);
        }
        if let Some(outcome) = self.drain_video(endpoint.session()) {
            return Some(outcome);
        }
        if let Some(outcome) = self.drain_audio(endpoint.session()) {
            return Some(outcome);
        }
        self.measure_lag(endpoint.session(), now_ms);
        self.publish(endpoint.session());
        None
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
            // The pointer, rumble and the roster's body are read by later
            // phases; here they are counted.
            op::CURSOR | op::RUMBLE | op::FRAME_TIMING => {}
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
            op::GUEST_LIST => self.number = Some(message.a1),
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
            let is_metadata = video::parse(content).is_ok_and(|header| header.metadata);
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

    /// Every sound packet, counted and described. Decoding and the playback
    /// window come with the sound phase.
    fn drain_audio(&mut self, session: &mut Session<'_>) -> Option<Outcome> {
        loop {
            let len = match session.take_message(AUDIO_CHANNEL, &mut self.sound) {
                None => return None,
                Some(Ok(len)) => len,
                Some(Err(error)) => {
                    lowlat_common::log_warn!("client: sound packet refused, error={error:?}");
                    return Some(Outcome::Unreadable);
                }
            };
            if let Ok(header) = audio::parse(self.sound.get(..len).unwrap_or(&[])) {
                if self.last_audio.is_none() {
                    lowlat_common::log_info!(
                        "client: sound mask={} samples={} rate={} codec={:?} channels={}",
                        header.mask,
                        header.samples,
                        header.rate,
                        header.codec,
                        header.channels
                    );
                }
                self.last_audio = Some(header);
            }
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

    fn publish(&self, session: &Session<'_>) {
        let t = &self.telemetry;
        t.pictures.store(self.pictures, Ordering::Relaxed);
        t.metadata.store(self.metadata, Ordering::Relaxed);
        t.skipped.store(self.skipped, Ordering::Relaxed);
        t.video_bytes.store(self.video_bytes, Ordering::Relaxed);
        t.audio_packets.store(self.audio_packets, Ordering::Relaxed);
        t.audio_bytes.store(self.audio_bytes, Ordering::Relaxed);
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
