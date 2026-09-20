//! Phase C1's gate: a whole session between this host's framing and this
//! client's driver, under the simulator.
//!
//! The host side is assembled from the host crate's own pieces -- its
//! negotiation, its packetiser, its control and sound writers -- over a
//! session wired as a guest's is; the client side is the driver the shell
//! thread runs, driven here with the simulator's clock instead. Every access
//! unit the host sends has to arrive whole, in order, with the keyframes
//! where the host said, and the two ends' counts of what they said to each
//! other have to agree opcode for opcode.
//!
//! The picture consumer is the feed over a recording fake by default: it
//! checks the units rather than the pictures; the clip tests put the real
//! decoder behind it. Sound is real both ways: the host encodes a tone, or
//! sends it uncompressed, and the client's own consumer decodes it.
//!
//! Thirty simulated seconds by default; `LOWLAT_HERMETIC_MS` runs longer.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lowlat_client::driver::{Driver, REPORT_INTERVAL_MS, Telemetry, Units};
use lowlat_client::feed::{Decision, Decoder, Fault, Fed, Feed};
use lowlat_client::sound::{Packets, Sound};
use lowlat_client::{AUDIO_CHANNEL, BODY, Config, Event, Outcome, VIDEO_CHANNEL};
use lowlat_common::events;
use lowlat_core::audio::{self, AudioHeader};
use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{self, Conn, Credentials};
use lowlat_core::control::{self, CONTROL_CHANNEL, Control, op};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_core::video::{Codec, Rotation, VideoHeader};
use lowlat_host::session::Negotiation;
use lowlat_host::video::Packetiser;
use lowlat_inject::event::{Device, Extents, Injector, Sink};
use lowlat_sim::{HostId, Link, Sim};

const LEFT: (&str, &str) = ("aaaaaaaa", "passwordforaaaaaaaaaaaaa");
const RIGHT: (&str, &str) = ("bbbbbbbb", "passwordforbbbbbbbbbbbbb");
const KEY: [u8; 32] = [0x5Au8; 32];

/// How often the loop wakes, in simulated milliseconds.
const TICK_MS: f64 = 1.0;
const FRAME_MS: f64 = 1000.0 / 60.0;
/// The host's real cadence: one frame of sound a packet.
const AUDIO_MS: f64 = 20.0;
const KEYFRAME_EVERY: u64 = 300;
const KEYFRAME_BYTES: usize = 300 * 1024;
const DELTA_BYTES: usize = 2048;
/// The tone the host sends: a different pitch and level per channel, so a
/// decoder that swapped or mixed them would read wrong.
const TONE: [(f64, f64); 2] = [(440.0, 12000.0), (660.0, 6000.0)];

const CONTROL_SLOTS: usize = 1024;
const VIDEO_SLOTS: usize = 4000;
const AUDIO_SLOTS: usize = 1024;

fn duration_ms() -> f64 {
    std::env::var("LOWLAT_HERMETIC_MS")
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(30_000.0)
}

fn addr(last: u8) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, last)), 5000)
}

/// Ring storage that outlives the test, so the sessions can be `'static`.
fn leak_recv(slots: usize) -> (&'static mut [u8], &'static mut [SlotMeta]) {
    (
        Box::leak(vec![0u8; BODY * slots].into_boxed_slice()),
        Box::leak(vec![SlotMeta::default(); slots].into_boxed_slice()),
    )
}

fn leak_send(slots: usize) -> (&'static mut [u8], &'static mut [SendSlot]) {
    (
        Box::leak(vec![0u8; BODY * slots].into_boxed_slice()),
        Box::leak(vec![SendSlot::default(); slots].into_boxed_slice()),
    )
}

fn conn(
    ours: (&'static str, &'static str),
    theirs: (&'static str, &'static str),
    seed: u8,
) -> Conn<'static> {
    Conn::new(
        Credentials {
            local_ufrag: ours.0,
            local_pwd: ours.1,
            remote_ufrag: theirs.0,
            remote_pwd: theirs.1,
        },
        [seed; 16],
        0.0,
    )
}

/// A session wired as this host wires a guest's: control both ways, video
/// and sound outbound.
fn host_session() -> Session<'static> {
    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    let (bodies, meta) = leak_recv(CONTROL_SLOTS);
    session
        .attach_recv(CONTROL_CHANNEL, RecvRing::new(bodies, meta, BODY).unwrap())
        .unwrap();
    for (channel, slots) in [
        (CONTROL_CHANNEL, 256),
        (VIDEO_CHANNEL, VIDEO_SLOTS),
        (AUDIO_CHANNEL, 128),
    ] {
        let (bodies, meta) = leak_send(slots);
        session
            .attach_send(channel, SendRing::new(bodies, meta, BODY, channel).unwrap())
            .unwrap();
    }
    session
}

/// A session wired as the client's thread wires its own.
fn client_session() -> Session<'static> {
    let mut session = Session::new(Envelope::from_key(&KEY).unwrap(), 1, 0.0);
    for (channel, slots) in [
        (CONTROL_CHANNEL, CONTROL_SLOTS),
        (VIDEO_CHANNEL, VIDEO_SLOTS),
        (AUDIO_CHANNEL, AUDIO_SLOTS),
    ] {
        let (bodies, meta) = leak_recv(slots);
        session
            .attach_recv(channel, RecvRing::new(bodies, meta, BODY).unwrap())
            .unwrap();
    }
    let (bodies, meta) = leak_send(256);
    session
        .attach_send(
            CONTROL_CHANNEL,
            SendRing::new(bodies, meta, BODY, CONTROL_CHANNEL).unwrap(),
        )
        .unwrap();
    session
}

/// The host's half: what this host does per guest, minus the capture and
/// the encoder, which a synthetic source stands in for.
struct Host {
    id: HostId,
    addr: SocketAddr,
    endpoint: Endpoint<'static, Session<'static>>,
    negotiation: Option<Negotiation>,
    packetiser: Packetiser,
    /// Messages received from the client, one count per opcode.
    received: [u32; 256],
    sent: [u32; 256],
    /// The client's last latency report per kind (video, sound), as the
    /// peer's argument order carries it: the figure first.
    reported_us: [u32; 3],
    frames: u64,
    audio: u64,
    next_frame_ms: f64,
    next_audio_ms: f64,
    inbound: Vec<u8>,
    /// Keyframes and pictures that were refused by a full window: a frame
    /// dropped by the sender is not one the client can be expected to see.
    refused: u64,
    /// Whether the stream is producing at all.
    streaming: bool,
    /// A real stream's access units in place of the synthetic ones, looped;
    /// its first unit is the keyframe.
    clip: Option<Vec<Vec<u8>>>,
    /// Whether keyframes are announced: `None` follows the guest's
    /// declaration, as this host does; `Some(false)` is a host that ignores
    /// it, which is what an older host is.
    announces: Option<bool>,
    /// The host's own expansion of the client's input, and what it produced.
    injector: Injector,
    injected: Injected,
    /// The sound encoder, and whether packets go out uncompressed instead.
    encoder: lowlat_audio::Encoder,
    raw_audio: bool,
    /// Every sample sent, in order, for the uncompressed comparison.
    sent_pcm: Vec<i16>,
}

/// Every device event the host's injector produced, in order, and every
/// pad report it handed to the device layer.
#[derive(Default)]
struct Injected {
    events: Vec<(Device, lowlat_inject::event::Event)>,
    unplugged: Vec<u32>,
    reports: Vec<(u32, String)>,
}

impl Sink for Injected {
    fn emit(&mut self, device: Device, events: &[lowlat_inject::event::Event]) {
        self.events
            .extend(events.iter().map(|event| (device, *event)));
    }

    fn unplug(&mut self, pad: u32) {
        self.unplugged.push(pad);
    }

    fn report(&mut self, pad: u32, inbound: &lowlat_core::pad::Inbound<'_>) {
        use lowlat_core::pad::Inbound;
        let what = match inbound {
            Inbound::Input { product, .. } => format!("input {product:?}"),
            Inbound::Feature {
                product, feature, ..
            } => format!("feature {product:?} {feature:?}"),
            Inbound::TouchBlock => "block".to_string(),
        };
        self.reports.push((pad, what));
    }
}

impl Host {
    fn new(sim: &mut Sim) -> Self {
        let addr = addr(20);
        let id = sim.add_host(addr, &[]);
        let mut packetiser = Packetiser::new(1920, 1080, Rotation::None, false);
        packetiser.set_colour(Codec::H264, false, false);
        Self {
            id,
            addr,
            endpoint: Endpoint::new(conn(RIGHT, LEFT, 0xB2), host_session()),
            negotiation: None,
            packetiser,
            received: [0; 256],
            sent: [0; 256],
            reported_us: [0; 3],
            frames: 0,
            audio: 0,
            next_frame_ms: 0.0,
            next_audio_ms: 0.0,
            inbound: vec![0u8; control::USER_DATA_MAX + control::CONTROL_HEADER_LEN],
            refused: 0,
            streaming: true,
            clip: None,
            announces: None,
            injector: Injector::new(Extents::alone(1920, 1080)),
            injected: Injected::default(),
            encoder: lowlat_audio::Encoder::new(lowlat_audio::encode::DEFAULT_BITRATE_KBPS)
                .unwrap(),
            raw_audio: false,
            sent_pcm: Vec::new(),
        }
    }

    /// Frames between keyframes: the clip's length, or the synthetic
    /// source's cadence.
    fn period(&self) -> u64 {
        self.clip
            .as_ref()
            .map_or(KEYFRAME_EVERY, |clip| clip.len() as u64)
    }

    /// The access unit for one frame number.
    fn unit(&self, frame: u64) -> (Vec<u8>, bool) {
        let keyframe = frame % self.period() == 0;
        match &self.clip {
            Some(clip) => (clip[(frame % self.period()) as usize].clone(), keyframe),
            None => (bitstream(frame, keyframe), keyframe),
        }
    }

    fn send_control(&mut self, message: &Control<'_>) {
        let mut header = [0u8; control::CONTROL_HEADER_LEN];
        control::encode_header(&mut header, message).unwrap();
        if self
            .endpoint
            .session()
            .send_message(CONTROL_CHANNEL, &header, message.body)
            .is_ok()
        {
            self.sent[usize::from(message.opcode)] += 1;
        }
    }

    /// One pass of what the guest loop does after the shell's turn.
    fn turn(&mut self, now: f64) {
        if let conn::State::Established(_) = self.endpoint.conn().state()
            && self.negotiation.is_none()
        {
            self.negotiation = Some(Negotiation::opened(now));
        }
        let Some(mut negotiation) = self.negotiation.take() else {
            return;
        };
        // Everything the client said.
        while let Some(result) = self
            .endpoint
            .session()
            .take_message(CONTROL_CHANNEL, &mut self.inbound)
        {
            let len = result.expect("a control message the host could not take");
            let message =
                control::parse(&self.inbound[..len]).expect("a malformed control message");
            self.received[usize::from(message.opcode)] += 1;
            if message.opcode == op::ENCODE_LATENCY {
                if let Some(slot) = self.reported_us.get_mut(message.a1 as usize) {
                    *slot = message.a0;
                }
            }
            self.injector.on_control(&message, &mut self.injected);
            let was_ready = negotiation.ready();
            negotiation.on_control(&message);
            if negotiation.ready() && !was_ready {
                // Seated: the packetiser's first keyframe is the encoder's
                // first, and the generation goes out with it.
                let asked = negotiation.asked().unwrap();
                self.packetiser
                    .set_announces(self.announces.unwrap_or(asked.announces_keyframes()));
                negotiation.encoder_initialised(self.packetiser.generation());
                self.next_frame_ms = now;
                self.next_audio_ms = now;
            }
            if negotiation.take_reconfigure() {
                // A request is an encoder rebuild: new sets, new generation.
                self.packetiser.reconfigured();
                negotiation.encoder_initialised(self.packetiser.generation());
                self.frames -= self.frames % self.period();
            }
        }
        if negotiation.ready() && self.streaming {
            while now >= self.next_frame_ms {
                self.next_frame_ms += FRAME_MS;
                self.send_frame(&mut negotiation);
            }
            while now >= self.next_audio_ms {
                self.next_audio_ms += AUDIO_MS;
                self.send_audio();
            }
        }
        if let Some(report) = negotiation.latency_report(now, 0) {
            self.send_control(&report);
        }
        self.negotiation = Some(negotiation);
    }

    fn send_frame(&mut self, negotiation: &mut Negotiation) {
        let (bitstream, keyframe) = self.unit(self.frames);
        if let Some(announcement) = self.packetiser.announcement(keyframe)
            && self
                .endpoint
                .session()
                .send_message(VIDEO_CHANNEL, announcement, &[])
                .is_err()
        {
            self.refused += 1;
            self.frames += 1;
            return;
        }
        let header = self.packetiser.header(keyframe).unwrap();
        if self
            .endpoint
            .session()
            .send_message(VIDEO_CHANNEL, header, &bitstream)
            .is_err()
        {
            self.refused += 1;
            self.frames += 1;
            return;
        }
        self.packetiser.sent(keyframe);
        self.frames += 1;
        let reports = negotiation.on_frame(3.0);
        if let Some(message) = reports.generation_message(0) {
            self.send_control(&message);
        }
    }

    fn send_audio(&mut self) {
        let frame = tone_frame(self.audio);
        let codec = if self.raw_audio {
            audio::Codec::Pcm
        } else {
            audio::Codec::Opus
        };
        let mut header = [0u8; audio::AUDIO_HEADER_LEN];
        audio::encode(
            &mut header,
            &AudioHeader::stereo(lowlat_audio::FRAME as u32, codec),
        )
        .unwrap();
        let encoded = self.encoder.encode(&frame).unwrap().to_vec();
        let payload = lowlat_audio::encode::payload_of(codec, &frame, &encoded);
        if self
            .endpoint
            .session()
            .send_message(AUDIO_CHANNEL, &header, payload)
            .is_ok()
        {
            self.audio += 1;
            self.sent_pcm.extend(
                frame
                    .chunks_exact(2)
                    .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
            );
        }
    }
}

/// One 20 ms frame of the tone, as capture would deliver it: interleaved
/// sixteen-bit stereo, continuous across frames.
fn tone_frame(packet: u64) -> Vec<u8> {
    let mut frame = Vec::with_capacity(lowlat_audio::FRAME_BYTES);
    for i in 0..lowlat_audio::FRAME {
        let t = (packet as usize * lowlat_audio::FRAME + i) as f64 / 48000.0;
        for (hz, amplitude) in TONE {
            let sample = ((2.0 * std::f64::consts::PI * hz * t).sin() * amplitude) as i16;
            frame.extend_from_slice(&sample.to_le_bytes());
        }
    }
    frame
}

/// A synthetic access unit: a parameter-set-led keyframe or a predicted
/// picture, carrying its own number so order and wholeness can be checked.
fn bitstream(frame: u64, keyframe: bool) -> Vec<u8> {
    let len = if keyframe {
        KEYFRAME_BYTES
    } else {
        DELTA_BYTES
    };
    let mut unit = vec![0u8; len];
    unit[..5].copy_from_slice(&[0, 0, 0, 1, if keyframe { 0x67 } else { 0x41 }]);
    unit[5..13].copy_from_slice(&frame.to_le_bytes());
    for (at, byte) in unit[13..].iter_mut().enumerate() {
        *byte = (frame as usize + at) as u8;
    }
    unit
}

/// What the feed's fake saw: every unit, in order, with what it was.
#[derive(Debug, Default)]
struct Recorder {
    units: Vec<(u64, bool)>,
    built: u32,
    torn: u32,
}

impl Decoder for Recorder {
    fn build(&mut self, header: &VideoHeader) -> Result<(), Fault> {
        assert_eq!(header.codec, Codec::H264);
        self.built += 1;
        Ok(())
    }
    fn feed(&mut self, unit: &[u8]) -> Result<Fed, Fault> {
        let keyframe = unit[4] == 0x67;
        let frame = u64::from_le_bytes(unit[5..13].try_into().unwrap());
        let expected = bitstream(frame, keyframe);
        assert_eq!(
            unit.len(),
            expected.len(),
            "frame {frame} arrived at the wrong length"
        );
        assert_eq!(unit, &expected[..], "frame {frame} arrived corrupted");
        self.units.push((frame, keyframe));
        Ok(Fed::Picture)
    }
    fn take(
        &mut self,
        _out: &mut lowlat_decode::Planes<'_>,
    ) -> Result<Option<lowlat_decode::Picture>, Fault> {
        Ok(None)
    }
    fn destroy(&mut self) {
        self.torn += 1;
    }
}

/// The client's half: the driver, its units, and the consumer.
struct Guest<D: Decoder> {
    id: HostId,
    addr: SocketAddr,
    endpoint: Endpoint<'static, Session<'static>>,
    driver: Driver,
    units: Units,
    feed: Feed<D>,
    events: events::Receiver<Event>,
    /// What the consumer is doing: taking every unit, or none.
    consuming: bool,
    /// How much simulated time one unit costs the consumer, so a decoder
    /// slower than the stream can be modelled; zero takes them as they come.
    cost_ms: f64,
    next_consume_ms: f64,
    metadata_seen: u64,
    audio_seen: u64,
    /// The sound consumer, drained after every pass as an application's
    /// thread would, and everything it handed over, in order.
    sound: Sound,
    telemetry: Arc<Telemetry>,
    heard: Vec<i16>,
    audio_acquired: u64,
    /// Decoders built, as the feed reported them.
    builds: u64,
    /// Plane checksums of the pictures a real decoder produced, in order.
    pictures: Vec<(u32, u32)>,
    planes: (Vec<u8>, Vec<u8>),
    /// The deepest lag the reader reached.
    deepest_lag: lowlat_client::Lag,
    /// The declaration generation last acted on, as the decode thread keeps
    /// it.
    reconfigured: u32,
}

impl<D: Decoder> Guest<D> {
    fn new(sim: &mut Sim, decoder: D) -> Self {
        let addr = addr(10);
        let id = sim.add_host(addr, &[]);
        let (emit, events) = events::queue();
        let units = Units::new();
        let packets = Packets::new();
        let telemetry = Arc::new(Telemetry::default());
        let driver = Driver::new(
            Config::default().init(&lowlat_decode::Caps::default()),
            units.clone(),
            packets.clone(),
            emit,
            Arc::clone(&telemetry),
        );
        Self {
            id,
            addr,
            endpoint: Endpoint::new(conn(LEFT, RIGHT, 0xA1), client_session()),
            driver,
            units,
            feed: Feed::new(decoder),
            events,
            consuming: true,
            cost_ms: 0.0,
            next_consume_ms: 0.0,
            metadata_seen: 0,
            audio_seen: 0,
            sound: Sound::new(packets, Arc::clone(&telemetry)),
            telemetry,
            heard: Vec::new(),
            audio_acquired: 0,
            builds: 0,
            pictures: Vec::new(),
            planes: (vec![0u8; 1280 * 720 * 2], vec![0u8; 1280 * 360 * 2]),
            deepest_lag: lowlat_client::Lag::default(),
            reconfigured: 0,
        }
    }

    fn turn(&mut self, now: f64) -> Option<Outcome> {
        let outcome = self.driver.turn(&mut self.endpoint, now);
        self.audio_seen = self.driver.audio_packets();
        let mut pcm = [0i16; lowlat_client::sound::FRAMES_MAX * 2];
        while let Ok(Some(acquired)) = self.sound.acquire(now, Duration::ZERO, &mut pcm) {
            self.audio_acquired += 1;
            self.heard.extend_from_slice(&pcm[..acquired.frames * 2]);
        }
        let lag = self.driver.lag();
        if lag.behind > self.deepest_lag.behind {
            self.deepest_lag = lag;
        }
        if self.consuming {
            self.consume(now);
        }
        outcome
    }

    fn consume(&mut self, now: f64) {
        // As the decode thread does: a changed declaration tears the
        // decoder down and asks for the keyframe, as one act.
        let generation = self.telemetry.reconfigure.load(Ordering::Acquire);
        if generation != self.reconfigured {
            self.reconfigured = generation;
            if self.feed.reconfigure() == Decision::Request {
                self.telemetry.request.store(true, Ordering::Release);
            }
        }
        loop {
            if self.cost_ms > 0.0 && now < self.next_consume_ms {
                return;
            }
            let Some(unit) = self.units.take() else {
                return;
            };
            if self.cost_ms > 0.0 {
                self.next_consume_ms = now.max(self.next_consume_ms) + self.cost_ms;
            }
            if unit.metadata() {
                self.metadata_seen += 1;
            }
            if let Some(generation) = self.driver.generation(0) {
                self.feed.announce_generation(generation);
            }
            let decision = self.feed.feed(unit.bytes());
            drop(unit);
            match decision {
                Decision::Built(_) => self.builds += 1,
                Decision::Fed(_) | Decision::Consumed(_) => {}
                // Between a teardown and the keyframe asked for, every
                // picture is ignored by rule.
                Decision::Ignored if !self.feed.present() => {}
                other => panic!("the feed refused a unit the host sent: {other:?}"),
            }
            // A real decoder has pictures to take; the fake has none.
            loop {
                let mut planes = lowlat_decode::Planes {
                    y: &mut self.planes.0,
                    y_pitch: 1280 * 2,
                    uv: &mut self.planes.1,
                    uv_pitch: 1280 * 2,
                    v: &mut [],
                    v_pitch: 0,
                };
                let Ok(Some(picture)) = self.feed.decoder_mut().take(&mut planes) else {
                    break;
                };
                let sample = picture.format.sample();
                let w = picture.width as usize * sample;
                let mut y = Vec::with_capacity(w * picture.height as usize);
                for row in 0..picture.height as usize {
                    y.extend_from_slice(&self.planes.0[row * 2560..row * 2560 + w]);
                }
                let mut uv = Vec::with_capacity(w * picture.height as usize / 2);
                for row in 0..picture.height as usize / 2 {
                    uv.extend_from_slice(&self.planes.1[row * 2560..row * 2560 + w]);
                }
                self.pictures.push((crc32(&y), crc32(&uv)));
            }
        }
    }
}

/// CRC-32 as the reference decoder's checksum tool computes it.
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, entry) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *entry = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

struct Pair<D: Decoder> {
    sim: Sim,
    host: Host,
    guest: Guest<D>,
}

impl Pair<Recorder> {
    fn new(seed: u64, link: Link) -> Self {
        Self::with(seed, link, Recorder::default(), None)
    }
}

impl<D: Decoder> Pair<D> {
    fn with(seed: u64, link: Link, decoder: D, clip: Option<Vec<Vec<u8>>>) -> Self {
        let mut sim = Sim::new(seed).with_link(link);
        let mut host = Host::new(&mut sim);
        host.clip = clip;
        let guest = Guest::new(&mut sim, decoder);
        let mut pair = Self { sim, host, guest };
        pair.guest
            .endpoint
            .conn()
            .add_candidate(pair.host.addr, conn::Kind::Reflexive)
            .unwrap();
        pair.host
            .endpoint
            .conn()
            .add_candidate(pair.guest.addr, conn::Kind::Reflexive)
            .unwrap();
        pair.guest.endpoint.conn().set_peer_ready();
        pair.host.endpoint.conn().set_peer_ready();
        pair
    }

    /// One tick: both ends drain into the path, the path delivers, time
    /// advances, both ends run their pass on the new time.
    fn tick(&mut self) -> Option<Outcome> {
        let now = self.sim.now_ms();
        let mut wire = [0u8; lowlat_core::MAX_DATAGRAM];
        let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
        for (id, endpoint) in [
            (self.host.id, &mut self.host.endpoint),
            (self.guest.id, &mut self.guest.endpoint),
        ] {
            while let Some(result) = endpoint.get_output(now, &mut wire) {
                let egress = result.expect("a malformed datagram was emitted");
                self.sim.send(id, egress.to, 64, &wire[..egress.len]);
            }
        }
        while let Some(arrival) = self.sim.next_arrival() {
            let endpoint = if arrival.host == self.host.id {
                &mut self.host.endpoint
            } else {
                &mut self.guest.endpoint
            };
            let _ = endpoint.process_input(&arrival.bytes, arrival.from, None, now, &mut scratch);
        }
        self.sim.advance_ms(TICK_MS);
        let now = self.sim.now_ms();
        self.host.endpoint.poll(now);
        self.guest.endpoint.poll(now);
        self.host.turn(now);
        self.guest.turn(now)
    }

    fn run_for(&mut self, ms: f64) {
        let until = self.sim.now_ms() + ms;
        while self.sim.now_ms() < until {
            if let Some(outcome) = self.tick() {
                panic!(
                    "the session ended: {outcome:?} at {:.0} ms",
                    self.sim.now_ms()
                );
            }
        }
    }

    fn established(&mut self) -> bool {
        self.guest.driver.established()
            && matches!(
                self.host.endpoint.conn().state(),
                conn::State::Established(_)
            )
    }
}

fn clean() -> Link {
    Link {
        one_way_ms: 20.0,
        ..Link::default()
    }
}

fn lossy() -> Link {
    Link {
        one_way_ms: 20.0,
        loss: 0.01,
        duplicate: 0.005,
        ..Link::default()
    }
}

fn reordering() -> Link {
    Link {
        one_way_ms: 20.0,
        reorder: 0.1,
        reorder_ms: 5.0,
        ..Link::default()
    }
}

/// The session end to end, and what has to be true at the end of it.
fn session_is_clean(seed: u64, link: Link) {
    session_is_clean_with(seed, link, false);
}

fn session_is_clean_with(seed: u64, link: Link, raw_audio: bool) {
    let mut pair = Pair::new(seed, link);
    pair.host.raw_audio = raw_audio;
    pair.run_for(2000.0);
    assert!(pair.established(), "the pair did not establish");
    pair.run_for(duration_ms());
    // Let the tail drain: the last frames sent are still in flight.
    pair.host.streaming = false;
    pair.run_for(2000.0);

    let host = &pair.host;
    let guest = &pair.guest;

    // **The start-up sequence, exactly.** The initialization, the diagnostics
    // opcode, a declaration for each secondary stream and none for the
    // first; the host read the fourteen keys and seated the guest on them.
    let negotiation = host
        .negotiation
        .as_ref()
        .expect("the host never opened a negotiation");
    let asked = negotiation
        .asked()
        .expect("the host never read the initialization");
    assert_eq!(asked.video_protocol_version, 1);
    assert_eq!(asked.max_width, 4096);
    assert_eq!(asked.channels, 2);
    assert!(!asked.has_preferred_size());
    assert_eq!(host.received[usize::from(op::INIT)], 1);
    assert_eq!(host.received[usize::from(op::DIAGNOSTICS)], 1);
    assert_eq!(host.received[usize::from(op::ENCODER_CONFIG)], 2);
    assert_eq!(host.received[usize::from(op::DISCONNECT)], 0);

    // **The census agrees message for message**, in both directions.
    for opcode in 0..=255u8 {
        assert_eq!(
            guest.driver.sent()[usize::from(opcode)],
            host.received[usize::from(opcode)],
            "opcode {opcode} ({}): the client sent a different number than the host received",
            op::name(opcode)
        );
        assert_eq!(
            host.sent[usize::from(opcode)],
            guest.driver.received()[usize::from(opcode)],
            "opcode {opcode} ({}): the host sent a different number than the client received",
            op::name(opcode)
        );
    }
    assert!(
        host.sent[usize::from(op::ENCODE_LATENCY)] >= 10,
        "no latency reports crossed"
    );
    // **The client reports both kinds on its clock**: two messages every two
    // seconds from establishment through the drain, give or take the pass
    // that lands on the boundary. Established for about two seconds before
    // the run, streaming for the run, then two of tail.
    let expected = 2 * ((duration_ms() + 4000.0) / REPORT_INTERVAL_MS).floor() as u32;
    let reports = host.received[usize::from(op::ENCODE_LATENCY)];
    assert!(
        (expected.saturating_sub(4)..=expected + 2).contains(&reports),
        "the client sent {reports} latency reports, expected about {expected}"
    );
    // The recorder decodes no picture, so the video figure stays zero and is
    // sent anyway; compressed sound is really decoded, so its figure is not
    // (an uncompressed packet is a copy that rounds to nothing).
    assert_eq!(
        host.reported_us[1], 0,
        "a decode time from a decoder that never ran"
    );
    if !raw_audio {
        assert!(host.reported_us[2] > 0, "no sound decode time was reported");
    }
    assert_eq!(host.sent[usize::from(op::ENCODER_GENERATION)], 1);
    assert_eq!(
        guest.driver.generation(0),
        Some(host.packetiser.generation())
    );
    assert!(
        guest.driver.encode_latency_us(0) > 0,
        "the encode latency was not stored"
    );

    // **Every access unit arrived, whole, in order, keyframes where the host
    // said.** Nothing was skipped: the reader was never behind.
    let host = &pair.host;
    let guest = &pair.guest;
    let units = &guest.feed.decoder().units;
    assert_eq!(host.refused, 0, "the host's window refused frames");
    assert_eq!(units.len() as u64, host.frames, "not every frame arrived");
    for (at, (frame, keyframe)) in units.iter().enumerate() {
        assert_eq!(*frame, at as u64, "frames arrived out of order at {at}");
        assert_eq!(*keyframe, at as u64 % KEYFRAME_EVERY == 0);
    }
    let keyframes = host.frames.div_ceil(KEYFRAME_EVERY);
    assert_eq!(
        guest.metadata_seen, keyframes,
        "a keyframe went unannounced"
    );
    assert_eq!(
        guest.feed.decoder().built,
        1,
        "an announced keyframe rebuilt the decoder"
    );
    assert_eq!(guest.driver.skipped(), 0);
    assert_eq!(guest.driver.pictures(), host.frames);

    // **Every sound packet arrived, was decoded, and was handed over**, none
    // dropped by the pool or refused by the decoder.
    assert_eq!(
        guest.audio_seen, host.audio,
        "not every sound packet arrived"
    );
    assert_eq!(
        guest.audio_acquired, host.audio,
        "not every sound packet was handed over"
    );
    let telemetry = &guest.telemetry;
    assert_eq!(telemetry.audio_dropped.load(Ordering::Relaxed), 0);
    assert_eq!(telemetry.audio_refused.load(Ordering::Relaxed), 0);
    assert_eq!(telemetry.audio_decoded.load(Ordering::Relaxed), host.audio);
    assert_eq!(
        guest.heard.len(),
        host.sent_pcm.len(),
        "the sound handed over is not the length of the sound sent"
    );
    if host.raw_audio {
        assert_eq!(
            telemetry.audio_codec.load(Ordering::Relaxed),
            u32::from(audio::Codec::Pcm as u8)
        );
        // Sample for sample, the whole run.
        assert!(guest.heard == host.sent_pcm, "uncompressed sound differs");
    } else {
        assert_eq!(
            telemetry.audio_codec.load(Ordering::Relaxed),
            u32::from(audio::Codec::Opus as u8)
        );
        // Compressed sound is not the samples that went in; it is the tone
        // at its level, per channel, over the last second once the codec
        // has settled -- a swapped or mixed channel reads wrong here.
        let last_second = &guest.heard[guest.heard.len() - 48000 * 2..];
        for (channel, (_, amplitude)) in TONE.iter().enumerate() {
            let energy: f64 = last_second
                .chunks_exact(2)
                .map(|pair| f64::from(pair[channel]) * f64::from(pair[channel]))
                .sum();
            let mean_square = energy / 48000.0;
            let expected = amplitude.powi(2) / 2.0;
            assert!(
                mean_square > expected * 0.8 && mean_square < expected * 1.25,
                "channel {channel}: mean square {mean_square}, expected about {expected}"
            );
        }
    }

    // Nothing was refused by either ring, and nothing is left behind.
    for channel in [CONTROL_CHANNEL, VIDEO_CHANNEL, AUDIO_CHANNEL] {
        let drops = pair
            .guest
            .endpoint
            .session()
            .recv_drops(channel)
            .unwrap_or_default();
        assert_eq!(
            drops.out_of_window, 0,
            "channel {channel} dropped out of window"
        );
        assert_eq!(drops.too_large, 0);
        assert!(!pair.guest.endpoint.session().has_gap(channel));
    }
    let lag = pair.guest.driver.lag();
    assert_eq!(lag.behind, 0, "the reader ended behind");

    // **What the receiver counted, against what the link did.** A clean
    // link with nothing reordered has no late arrival and no negative to
    // send; one percent of loss has both, and the recent-loss figure on the
    // video channel reads on the order of the loss rate after the run
    // (an average with a thirtieth's weight reaches 0.63 of the rate in
    // thirty seconds and 0.86 in sixty).
    let t = &pair.guest.telemetry;
    let video = usize::from(VIDEO_CHANNEL);
    let fragments = t.fragments[video].load(Ordering::Relaxed);
    let late = t.late[video].load(Ordering::Relaxed);
    let nacks = t.nacks_sent[video].load(Ordering::Relaxed);
    let loss = f32::from_bits(t.loss_30s[video].load(Ordering::Relaxed));
    assert!(
        fragments > 1000,
        "too few video fragments counted: {fragments}"
    );
    assert_eq!(
        t.messages[video].load(Ordering::Relaxed),
        pair.host.frames + pair.guest.metadata_seen,
        "the video channel's message count is not the frames and their announcements"
    );
    assert!(t.bytes[video].load(Ordering::Relaxed) > 0);
    assert!(t.connected_ms.load(Ordering::Relaxed) as f64 >= duration_ms());
    if link.loss == 0.0 && link.reorder == 0.0 {
        assert_eq!(late, 0, "a clean link delivered late");
        assert_eq!(nacks, 0, "a clean link was asked for a retransmission");
        assert_eq!(loss.to_bits(), 0, "a clean link reads loss: {loss}");
    } else if link.loss > 0.0 {
        assert!(late > 0, "loss repaired without a late arrival");
        assert!(nacks > 0, "loss repaired without a negative");
        let expected = link.loss as f32;
        assert!(
            loss > expected * 0.3 && loss < expected * 2.0,
            "recent loss {loss} against a link at {expected}"
        );
    }
}

#[test]
fn a_session_is_clean_at_zero_loss() {
    session_is_clean(1, clean());
}

#[test]
fn a_session_is_clean_at_one_percent_loss() {
    session_is_clean(2, lossy());
}

#[test]
fn a_session_is_clean_under_five_milliseconds_of_reorder() {
    session_is_clean(3, reordering());
}

/// The other codec: uncompressed sound comes out sample for sample, under
/// the lossy link so the retransmissions are in the path too.
#[test]
fn uncompressed_sound_arrives_sample_for_sample() {
    session_is_clean_with(2, lossy(), true);
}

/// **The catch-up lands on an announced keyframe, and it runs on the
/// receive side.** With the consumer stalled the backlog grows in the
/// receive ring only until the next keyframe's metadata arrives with its
/// picture; the pictures before it are then discarded and counted as
/// skipped, so the reader is never more than one keyframe interval behind.
/// When it resumes, the picture continues from the keyframe and the only
/// discontinuities are those jumps.
#[test]
fn a_reader_behind_catches_up_at_the_next_announced_keyframe() {
    let mut pair = Pair::new(4, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.run_for(1000.0);
    let seen_before = pair.guest.feed.decoder().units.len();
    assert!(seen_before > 30);

    // Stall the consumer across two keyframes' worth of stream, watching
    // the lag as it goes.
    pair.guest.consuming = false;
    let mut deepest = 0u32;
    let mut oldest_ms = 0u32;
    for _ in 0..25 {
        pair.run_for(FRAME_MS * KEYFRAME_EVERY as f64 / 10.0);
        let lag = pair.guest.driver.lag();
        deepest = deepest.max(lag.behind);
        oldest_ms = oldest_ms.max(lag.behind_ms);
    }
    assert!(
        deepest > 100,
        "the reader was never measured behind: {deepest}"
    );
    assert!(
        u64::from(deepest) <= KEYFRAME_EVERY + lowlat_client::UNIT_SLOTS as u64 + 2,
        "the backlog grew past a keyframe interval: {deepest}"
    );
    assert!(
        oldest_ms > 10_000,
        "the lag's age was not measured: {oldest_ms}"
    );
    let skipped = pair.guest.driver.skipped();
    assert!(skipped > 2 * 100, "too little was skipped: {skipped}");

    pair.guest.consuming = true;
    pair.run_for(500.0);
    // The consumer sees one jump: what was handed over before the stall,
    // then the latest keyframe, since the earlier one was discarded by the
    // catch-up that followed it.
    let units = &pair.guest.feed.decoder().units;
    let mut jumps = Vec::new();
    for pair_of in units.windows(2) {
        let (before, after) = (pair_of[0].0, pair_of[1].0);
        if after != before + 1 {
            assert!(
                pair_of[1].1,
                "the jump landed on frame {after}, not a keyframe"
            );
            jumps.push(after);
        }
    }
    assert_eq!(jumps.len(), 1, "the picture jumped at {jumps:?}");
    assert_eq!(jumps[0] % KEYFRAME_EVERY, 0);
    assert!(
        jumps[0] >= 2 * KEYFRAME_EVERY,
        "landed on the first keyframe behind rather than the latest: {}",
        jumps[0]
    );
    assert_eq!(pair.guest.driver.lag().behind, 0);
    assert_eq!(
        pair.guest.driver.skipped(),
        skipped,
        "resuming skipped more"
    );
}

/// **Nothing is skipped when no keyframe is ahead.** A reader behind by a
/// backlog of predicted pictures decodes every one of them in order,
/// because none can be skipped to.
#[test]
fn nothing_is_skipped_when_no_keyframe_is_ahead() {
    let mut pair = Pair::new(5, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    // Past the first keyframe, then stall for less than a keyframe interval.
    pair.run_for(200.0);
    pair.guest.consuming = false;
    pair.run_for(FRAME_MS * 100.0);
    assert!(pair.guest.driver.lag().behind > 50);
    pair.guest.consuming = true;
    pair.run_for(200.0);
    assert_eq!(
        pair.guest.driver.skipped(),
        0,
        "a backlog without a keyframe was skipped"
    );
    let units = &pair.guest.feed.decoder().units;
    for (at, (frame, _)) in units.iter().enumerate() {
        assert_eq!(*frame, at as u64, "the backlog was not decoded in order");
    }
}

/// **Nothing is skipped over a gap.** A keyframe whose tail is still in
/// flight is not a keyframe the reader can land on: the look-ahead stops at
/// the first incomplete message.
#[test]
fn nothing_is_skipped_over_a_gap() {
    // A link that holds datagrams back long enough that a keyframe's tail is
    // routinely behind the pictures after it.
    let link = Link {
        one_way_ms: 20.0,
        reorder: 0.2,
        reorder_ms: 60.0,
        ..Link::default()
    };
    let mut pair = Pair::new(6, link);
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.guest.consuming = false;
    pair.run_for(FRAME_MS * KEYFRAME_EVERY as f64 * 1.5);
    pair.guest.consuming = true;
    pair.run_for(2000.0);
    pair.host.streaming = false;
    pair.run_for(2000.0);
    // Whatever was skipped, what was decoded is whole and in order, and
    // resumes only at a keyframe.
    let units = &pair.guest.feed.decoder().units;
    for pair_of in units.windows(2) {
        if pair_of[1].0 != pair_of[0].0 + 1 {
            assert!(
                pair_of[1].1,
                "resumed at frame {}, not a keyframe",
                pair_of[1].0
            );
        }
    }
    assert!(!pair.guest.endpoint.session().has_gap(VIDEO_CHANNEL));
}

/// The host ends the session with a status; the client reports it and
/// stops. A disconnect carrying zero is not an ending.
#[test]
fn a_disconnect_from_the_host_ends_the_session_with_its_status() {
    let mut pair = Pair::new(7, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.host.send_control(&Control {
        a0: 0,
        a1: 0,
        a2: 0,
        opcode: op::DISCONNECT,
        body: &[],
    });
    pair.run_for(500.0);
    pair.host.send_control(&Control {
        a0: 11u32,
        a1: 0,
        a2: 0,
        opcode: op::DISCONNECT,
        body: &[],
    });
    let mut ended = None;
    for _ in 0..1000 {
        if let Some(outcome) = pair.tick() {
            ended = Some(outcome);
            break;
        }
    }
    assert_eq!(ended, Some(Outcome::Disconnected(11)));
}

/// A clean departure is opcode 10 with a zero status, and the host reads it.
#[test]
fn a_departure_reaches_the_host_as_a_zero_disconnect() {
    let mut pair = Pair::new(8, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    let now = pair.sim.now_ms();
    pair.guest.driver.leave(pair.guest.endpoint.session(), now);
    pair.run_for(300.0);
    assert!(pair.guest.driver.left(pair.sim.now_ms()));
    assert_eq!(pair.host.received[usize::from(op::DISCONNECT)], 1);
    let _ = pair.guest.events.try_recv();
}

/// **A preference changed mid-session is one restatement per secondary
/// stream, one request on the first, one teardown and one build**, and the
/// picture goes on: the host reads the new flags and answers with a keyframe.
#[test]
fn a_preference_changed_mid_session_costs_one_request_and_one_build() {
    let mut pair = Pair::new(11, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.run_for(5000.0);
    let before = pair.host.received[usize::from(op::ENCODER_CONFIG)];
    let builds = pair.guest.builds;
    let pictures = pair.guest.driver.pictures();
    assert_eq!(before, 2, "the two secondary declarations at the start");
    assert_eq!(builds, 1);

    // The second codec asked for, as the seam would push it after the mask.
    let flags = lowlat_core::init::FLAG_BASE | lowlat_core::init::FLAG_HEVC;
    pair.guest
        .driver
        .set_flags(pair.guest.endpoint.session(), flags);
    pair.run_for(5000.0);

    let negotiation = pair.host.negotiation.as_ref().unwrap();
    assert_eq!(
        pair.host.received[usize::from(op::ENCODER_CONFIG)],
        before + 3,
        "two restatements and one request"
    );
    assert_eq!(
        negotiation.flags(),
        flags,
        "the host read the new declaration"
    );
    assert_eq!(pair.guest.builds, builds + 1, "one teardown, one build");
    assert!(
        pair.guest.driver.pictures() > pictures + 100,
        "the picture did not go on"
    );
    // The same flags again change nothing and send nothing.
    let sent = pair.guest.driver.sent()[usize::from(op::ENCODER_CONFIG)];
    pair.guest
        .driver
        .set_flags(pair.guest.endpoint.session(), flags);
    pair.run_for(1000.0);
    assert_eq!(
        pair.guest.driver.sent()[usize::from(op::ENCODER_CONFIG)],
        sent
    );
}

/// **Input crosses as the host reads it.** The rectangle is the picture's
/// own size, so the far window pixel bumps onto the far edge and the host's
/// injector puts the pointer at the end of its axis; a press outside is not
/// sent and its release is; a repeated pad state is sent once; the census
/// still agrees message for message. And the host's pointer message moves
/// the client into relative mode and back, once each, with the warp position
/// on the way out.
#[test]
fn input_reaches_the_host_in_the_pictures_pixels() {
    use lowlat_client::input::{Input, PadState, Viewport};
    use lowlat_core::cursor;

    let mut pair = Pair::new(21, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    // A picture has to have arrived for the mapper to know its size.
    pair.run_for(200.0);
    assert!(pair.guest.driver.pictures() > 0);
    pair.guest.driver.set_viewport(Viewport {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    });

    let pad = PadState {
        buttons: 0x1001,
        lx: 100,
        ly: -100,
        rx: 0,
        ry: 0,
        lt: 0,
        rt: 255,
    };
    let reports = [
        Input::Key {
            code: 0,
            mods: 0,
            pressed: true,
        },
        Input::Key {
            code: 4,
            mods: 0x2000,
            pressed: true,
        },
        Input::Key {
            code: 4,
            mods: 0x2000,
            pressed: false,
        },
        Input::Motion {
            x: 1919,
            y: 1079,
            relative: false,
        },
        Input::Button {
            button: 1,
            pressed: true,
            x: 5000,
            y: 5,
        },
        Input::Button {
            button: 1,
            pressed: true,
            x: 5,
            y: 5,
        },
        Input::Button {
            button: 1,
            pressed: false,
            x: 5000,
            y: 5,
        },
        Input::Wheel { x: 0, y: -120 },
        Input::PadState { pad: 7, state: pad },
        Input::PadState { pad: 7, state: pad },
        Input::PadUnplug { pad: 7 },
        Input::ReleaseAll,
    ];
    for report in &reports {
        pair.guest
            .driver
            .send_input(pair.guest.endpoint.session(), report);
    }
    pair.run_for(300.0);

    let host = &pair.host;
    assert_eq!(host.received[usize::from(op::KEYBOARD)], 2);
    assert_eq!(host.received[usize::from(op::MOUSE_MOTION)], 1);
    assert_eq!(host.received[usize::from(op::MOUSE_BUTTON)], 2);
    assert_eq!(host.received[usize::from(op::MOUSE_WHEEL)], 1);
    assert_eq!(host.received[usize::from(op::GAMEPAD_STATE)], 1);
    assert_eq!(host.received[usize::from(op::GAMEPAD_UNPLUG)], 1);
    assert_eq!(host.received[usize::from(op::RELEASE)], 1);
    for opcode in 0..=255u8 {
        assert_eq!(
            pair.guest.driver.sent()[usize::from(opcode)],
            host.received[usize::from(opcode)],
            "opcode {opcode} ({})",
            op::name(opcode)
        );
    }
    // The host's injector put the pointer at the end of both axes: the
    // window's last pixel was bumped onto the picture's edge.
    let absolute: Vec<(u16, i32)> = host
        .injected
        .events
        .iter()
        .filter(|(device, event)| *device == Device::PointerAbsolute && event.kind == 0x03)
        .map(|(_, event)| (event.code, event.value))
        .collect();
    assert_eq!(absolute, vec![(0x00, 65535), (0x01, 65535)]);
    assert_eq!(host.injected.unplugged, vec![7]);
    assert_eq!(host.injector.tally().pads, 2);

    // The host hides the pointer: relative mode, once, on the transition;
    // then shows it at the picture's centre, and the warp position comes
    // back in the window's units.
    let mut out = vec![0u8; cursor::encoded_len(0)];
    let hidden = cursor::Update {
        hidden: true,
        ..cursor::Update::default()
    };
    let used = cursor::encode(&mut out, &hidden, cursor::Image::Unchanged, false).unwrap();
    let message = control::parse(&out[..used]).unwrap();
    pair.host.send_control(&message);
    pair.host.send_control(&message);
    pair.run_for(100.0);
    let shown = cursor::Update {
        x: 960,
        y: 540,
        ..cursor::Update::default()
    };
    let used = cursor::encode(&mut out, &shown, cursor::Image::Unchanged, false).unwrap();
    let message = control::parse(&out[..used]).unwrap();
    pair.host.send_control(&message);
    pair.run_for(100.0);
    let mut transitions = Vec::new();
    while let Some(event) = pair.guest.events.try_recv() {
        if let Event::Relative { relative, x, y } = event.event {
            transitions.push((relative, x, y));
        }
    }
    assert_eq!(transitions, vec![(true, 0, 0), (false, 960, 540)]);
}

/// A pad's own reports reach the host as the report and the state it
/// implies -- a DualShock 4's as its body, the touch block and the state --
/// and what the host's device is written comes back as an event framed for
/// the pad: as it is for a USB pad, with the wireless identifier, sequence
/// and checksum for one reported over Bluetooth; a write for a pad never
/// sent as reports is dropped and counted.
#[test]
fn a_pads_own_reports_reach_the_host_and_its_writes_come_back() {
    use lowlat_client::input::{Input, ReportKind};
    use lowlat_core::pad::{self, OutputKind, Product};

    let ds5_idle: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-idle.bin");
    let ds5_held: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds5/input-held.bin");
    let ds4_idle: &[u8; 64] = include_bytes!("../../core/tests/data/pad/ds4/input-idle.bin");
    let calibration = include_bytes!("../../core/tests/data/pad/ds5/feature-calibration.bin");

    let mut pair = Pair::new(29, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.run_for(200.0);

    let report = |pad, product, kind, transport, bytes: &[u8]| {
        let mut report = [0u8; pad::INPUT_LEN];
        report[..bytes.len()].copy_from_slice(bytes);
        Input::PadReport {
            pad,
            product,
            kind,
            transport,
            len: bytes.len() as u8,
            report,
        }
    };
    let inputs = [
        report(
            3,
            Product::DualSense,
            ReportKind::Feature,
            pad::Transport::Usb,
            calibration,
        ),
        report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            ds5_idle,
        ),
        report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            ds5_idle,
        ),
        report(
            3,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Usb,
            ds5_held,
        ),
        report(
            5,
            Product::DualShock4,
            ReportKind::Input,
            pad::Transport::Usb,
            ds4_idle,
        ),
        report(
            7,
            Product::DualSense,
            ReportKind::Input,
            pad::Transport::Bluetooth,
            ds5_idle,
        ),
    ];
    for input in &inputs {
        pair.guest
            .driver
            .send_input(pair.guest.endpoint.session(), input);
    }
    pair.run_for(300.0);
    let host = &pair.host;
    // One feature, four DualSense inputs, a DualShock 4 body and its block.
    assert_eq!(host.received[usize::from(op::PAD_REPORT)], 7);
    // The states: two for pad 3 (idle, then held), one each for 5 and 7.
    assert_eq!(host.received[usize::from(op::GAMEPAD_STATE)], 4);
    assert_eq!(
        pair.guest
            .telemetry
            .pad_reports_sent
            .load(Ordering::Relaxed),
        6
    );
    // The host reads the reports: each pad is made from its first one, the
    // feature report kept ahead of it, the touch block creating nothing, and
    // the states beside them dropped -- no sixteen-button device for any of
    // the three, however many state messages arrived (the tally counts
    // them before the family rule does).
    assert_eq!(host.injector.tally().pads, 4);
    let pads: Vec<u32> = host
        .injected
        .events
        .iter()
        .filter_map(|(device, _)| match device {
            Device::Gamepad(id) => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(pads, Vec::<u32>::new());
    assert_eq!(
        host.injected.reports,
        vec![
            (3, "feature DualSense Calibration".to_string()),
            (3, "input DualSense".to_string()),
            (3, "input DualSense".to_string()),
            (3, "input DualSense".to_string()),
            (5, "input DualShock4".to_string()),
            (7, "input DualSense".to_string()),
        ]
    );

    // What the host's devices are written, back to the pads.
    let mut out = [0u8; pad::DS5_OUTPUT_MIN_LEN];
    out[0] = pad::DS5_OUTPUT_ID;
    out[1] = 0x03;
    out[3] = 200;
    out[4] = 100;
    out[45..48].copy_from_slice(&[0, 255, 0]);
    let mut buf = [0u8; 128];
    for (pad, kind) in [
        (3, OutputKind::Output),
        (7, OutputKind::Output),
        (99, OutputKind::Output),
        (3, OutputKind::Feature),
    ] {
        let n = pad::encode_output(&mut buf, pad, kind, &out).unwrap();
        let message = control::parse(&buf[..n]).unwrap();
        pair.host.send_control(&message);
    }
    pair.run_for(100.0);
    let mut back = Vec::new();
    while let Some(event) = pair.guest.events.try_recv() {
        if let Event::PadReport {
            pad,
            kind,
            len,
            report,
        } = event.event
        {
            back.push((pad, kind, report[..usize::from(len)].to_vec()));
        }
    }
    assert_eq!(back.len(), 3);
    assert_eq!(back[0].0, 3);
    assert_eq!(back[0].1, OutputKind::Output);
    assert_eq!(back[0].2, out.to_vec());
    assert_eq!(back[1].0, 7);
    assert_eq!(back[1].2.len(), pad::BT_OUTPUT_LEN);
    assert_eq!(&back[1].2[..3], &[0x31, 0x00, 0x10]);
    assert_eq!(&back[1].2[3..50], &out[1..]);
    assert_eq!(back[2], (3, OutputKind::Feature, out.to_vec()));
    assert_eq!(
        pair.guest
            .telemetry
            .pad_reports_received
            .load(Ordering::Relaxed),
        3
    );
    assert_eq!(
        pair.guest
            .telemetry
            .pad_reports_dropped
            .load(Ordering::Relaxed),
        1
    );
}

/// Everything else the host says on the control channel comes out as an
/// event, in order, with the right bytes: the pointer's picture fresh and
/// then by name, a name after a forget (a miss, delivered without the
/// picture), a name carrying no size (the size and hotspot stored with the
/// picture), rumble, blocked and unblocked, host mode, the guest list with
/// this client's own number.
#[test]
fn the_hosts_control_messages_become_events() {
    use lowlat_core::cursor;

    let mut pair = Pair::new(23, clean());
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.run_for(200.0);
    pair.guest
        .driver
        .set_viewport(lowlat_client::input::Viewport {
            x: 10,
            y: 20,
            w: 960,
            h: 540,
        });

    // A pointer picture, written by the encoder the host uses.
    let width = 12u32;
    let height = 9u32;
    let pixels: Vec<u8> = (0..width * height * 4).map(|i| (i * 7) as u8).collect();
    let mut png = vec![0u8; lowlat_core::png::upper_bound(width, height)];
    let used =
        lowlat_core::png::encode(&pixels, width, height, (width * 4) as usize, &mut png).unwrap();
    png.truncate(used);
    let checksum = crc32(&png);

    let fresh = cursor::Update {
        x: 480,
        y: 270,
        hot_x: 3,
        hot_y: 4,
        ..cursor::Update::default()
    };
    let named = cursor::Update {
        x: 100,
        y: 50,
        hot_x: 3,
        hot_y: 4,
        suppressed: true,
        ..cursor::Update::default()
    };
    let mut out = vec![0u8; cursor::encoded_len(png.len())];
    let mut send = |pair: &mut Pair<Recorder>, update: &cursor::Update, image, forget| {
        let used = cursor::encode(&mut out, update, image, forget).unwrap();
        let message = control::parse(&out[..used]).unwrap();
        pair.host.send_control(&message);
    };
    send(
        &mut pair,
        &fresh,
        cursor::Image::Fresh {
            png: &png,
            width: width as u16,
            height: height as u16,
            checksum,
        },
        false,
    );
    send(&mut pair, &named, cursor::Image::Cached { checksum }, false);
    // Forget, then name the picture again: a miss.
    send(&mut pair, &named, cursor::Image::Cached { checksum }, true);
    pair.run_for(100.0);

    let rumble = Control {
        a0: 7,
        a1: 0x1FF,
        a2: 0x80,
        opcode: op::RUMBLE,
        body: &[],
    };
    pair.host.send_control(&rumble);
    for (blocked, opcode) in [(1u32, op::BLOCKED), (0, op::BLOCKED)] {
        pair.host.send_control(&Control {
            a0: blocked,
            a1: 0,
            a2: 0,
            opcode,
            body: &[],
        });
    }
    pair.host.send_control(&Control {
        a0: 1,
        a1: 0,
        a2: 0,
        opcode: op::HOST_MODE,
        body: &[],
    });
    let roster = b"[{\"id\":5,\"owner\":true}]\0";
    pair.host.send_control(&Control {
        a0: roster.len() as u32,
        a1: 5,
        a2: 0,
        opcode: op::GUEST_LIST,
        body: roster,
    });
    pair.run_for(100.0);

    let mut events = Vec::new();
    while let Some(event) = pair.guest.events.try_recv() {
        events.push(event.event);
    }
    let cursors: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Cursor {
                x,
                y,
                width,
                height,
                hot_x,
                hot_y,
                suppressed,
                checksum,
                png,
                ..
            } => Some((
                *x,
                *y,
                *width,
                *height,
                *hot_x,
                *hot_y,
                *suppressed,
                *checksum,
                png.clone(),
            )),
            _ => None,
        })
        .collect();
    // The picture is 1920x1080 drawn at half size from (10, 20): the
    // position comes out in the window's units.
    assert_eq!(cursors.len(), 3, "{cursors:?}");
    assert_eq!(
        cursors[0],
        (250, 155, 12, 9, 3, 4, false, checksum, png.clone())
    );
    // Named with no size: the size and hotspot come from what was stored,
    // and the picture already delivered travels as its name alone.
    assert_eq!(
        cursors[1],
        (60, 45, 12, 9, 3, 4, true, checksum, Vec::new())
    );
    // Forgotten, then named: no picture, the rest delivered.
    assert_eq!(cursors[2], (60, 45, 0, 0, 3, 4, true, 0, Vec::new()));
    let t = &pair.guest.telemetry;
    assert_eq!(t.cursor_images.load(Ordering::Relaxed), 1);
    assert_eq!(t.cursor_misses.load(Ordering::Relaxed), 1);

    let rest: Vec<_> = events
        .iter()
        .filter(|e| !matches!(e, Event::Cursor { .. } | Event::Established { .. }))
        .collect();
    assert_eq!(
        rest,
        vec![
            &Event::Rumble {
                pad: 7,
                large: 0xFF,
                small: 0x80
            },
            &Event::Blocked { blocked: true },
            &Event::Blocked { blocked: false },
            &Event::HostMode { mode: 1 },
            &Event::GuestList {
                number: 5,
                body: roster[..roster.len() - 1].to_vec()
            },
        ]
    );
    assert_eq!(t.number.load(Ordering::Relaxed), 5);
}

/// The committed clips, as the decode crate's tests read them.
fn clip(name: &str) -> Vec<Vec<u8>> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../decode/tests/data")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = Vec::new();
    let mut at = 0;
    while at + 4 <= bytes.len() {
        let len =
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        at += 4;
        out.push(bytes[at..at + len].to_vec());
        at += len;
    }
    out
}

fn sums(name: &str) -> Vec<(u32, u32)> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../decode/tests/data")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut f = l
                .split_whitespace()
                .skip(1)
                .map(|v| v.parse::<u32>().unwrap());
            (f.next().unwrap(), f.next().unwrap())
        })
        .collect()
}

/// **The hermetic session decodes.** This host's own framing carries a real
/// stream to a real decoder, and every picture out is the picture the
/// reference decoder produced, frame for frame, at zero loss. Needs the
/// open-stack driver on a render node, so it is off by default:
/// `cargo test -p lowlat-client --test hermetic -- --ignored`.
#[test]
#[ignore = "requires the open-stack driver"]
fn the_session_decodes_the_clip_frame_for_frame() {
    let node = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let node = std::ffi::CString::new(node).unwrap();
    let va = lowlat_decode::vaapi::Vaapi::load().expect("runtime");
    let display = va.open(&node).expect("render node");
    let backend = lowlat_decode::vaapi::Backend::new(&display, (1280, 720));
    let units = clip("synthetic-720p-h264.bin");
    let expected = sums("synthetic-720p-h264.sums");
    let period = units.len() as u64;

    let mut pair = Pair::with(9, clean(), backend, Some(units));
    pair.run_for(2000.0);
    assert!(pair.established());
    // Three loops of the clip: three keyframes, two of them announced
    // mid-stream.
    pair.run_for(FRAME_MS * period as f64 * 3.0);
    pair.host.streaming = false;
    pair.run_for(2000.0);

    let frames = pair.host.frames;
    assert_eq!(pair.host.refused, 0);
    assert_eq!(
        pair.guest.driver.skipped(),
        0,
        "the reader fell behind a real decoder"
    );
    assert_eq!(
        pair.guest.pictures.len() as u64,
        frames,
        "a picture in did not come out"
    );
    for (n, got) in pair.guest.pictures.iter().enumerate() {
        let want = expected[n % expected.len()];
        assert_eq!(
            *got, want,
            "picture {n} differs from the reference decoder's"
        );
    }
    assert_eq!(
        pair.guest.builds, 1,
        "an announced keyframe rebuilt the decoder"
    );
    assert!(
        pair.guest.feed.decoder().decode_us > 0,
        "no decode was timed"
    );
    println!(
        "hermetic decode: {frames} pictures frame-for-frame, last decode {} us, readback {} us",
        pair.guest.feed.decoder().decode_us,
        pair.guest.feed.decoder().readback_us
    );
}

/// **Under the older framing every keyframe rebuilds the decoder and the
/// picture continues** (docs/impl-plan-client.md C2 gate 3): the same clip
/// from a host that ignores the declaration and announces nothing, so each
/// parameter-set-led unit tears the decoder down and builds afresh, and every
/// picture across those rebuilds still matches the reference decoder's.
#[test]
#[ignore = "requires the open-stack driver"]
fn the_session_decodes_the_clip_under_the_older_framing() {
    let node = std::env::var("LOWLAT_VAAPI_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let node = std::ffi::CString::new(node).unwrap();
    let va = lowlat_decode::vaapi::Vaapi::load().expect("runtime");
    let display = va.open(&node).expect("render node");
    let backend = lowlat_decode::vaapi::Backend::new(&display, (1280, 720));
    let units = clip("synthetic-720p-h264.bin");
    let expected = sums("synthetic-720p-h264.sums");
    let period = units.len() as u64;

    let mut pair = Pair::with(12, clean(), backend, Some(units));
    pair.host.announces = Some(false);
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.run_for(FRAME_MS * period as f64 * 3.0);
    pair.host.streaming = false;
    pair.run_for(2000.0);

    let frames = pair.host.frames;
    let keyframes = frames.div_ceil(period);
    assert_eq!(pair.host.refused, 0);
    assert_eq!(pair.guest.metadata_seen, 0, "an older host announced");
    assert_eq!(
        pair.guest.builds, keyframes,
        "a keyframe under the older framing did not rebuild"
    );
    assert_eq!(pair.guest.driver.skipped(), 0);
    assert_eq!(
        pair.guest.pictures.len() as u64,
        frames,
        "a picture was lost across a rebuild"
    );
    for (n, got) in pair.guest.pictures.iter().enumerate() {
        let want = expected[n % expected.len()];
        assert_eq!(
            *got, want,
            "picture {n} differs from the reference decoder's"
        );
    }
    println!("older framing: {frames} pictures frame-for-frame across {keyframes} rebuilds");
}

/// **The lag a reader reaches when its decoder is half the stream's rate**,
/// recorded rather than judged (docs/impl-plan-client.md C2 gate 1). Against
/// a host that announces every keyframe the catch-up bounds the backlog at
/// one keyframe interval; the figures are what the deferred decisions are
/// decided on.
#[test]
fn a_decoder_at_half_the_rate_reaches_a_lag_the_keyframes_bound() {
    let mut pair = Pair::new(10, clean());
    pair.guest.cost_ms = FRAME_MS * 2.0;
    pair.run_for(2000.0);
    assert!(pair.established());
    pair.run_for(FRAME_MS * KEYFRAME_EVERY as f64 * 4.0);
    let deepest = pair.guest.deepest_lag;
    let skipped = pair.guest.driver.skipped();
    println!(
        "half-rate decoder: deepest lag {} messages / {} ms, {skipped} pictures skipped by the catch-up over {} frames",
        deepest.behind, deepest.behind_ms, pair.host.frames
    );
    // The backlog never grows past one keyframe interval plus the pool.
    assert!(
        u64::from(deepest.behind) <= KEYFRAME_EVERY + lowlat_client::UNIT_SLOTS as u64 + 2,
        "the lag grew past a keyframe interval: {deepest:?}"
    );
    assert!(skipped > 0, "a reader at half rate never caught up");
    // And a reader that keeps up reaches no lag worth the name.
    let mut pair = Pair::new(11, clean());
    pair.run_for(2000.0);
    pair.run_for(FRAME_MS * KEYFRAME_EVERY as f64 * 2.0);
    assert!(
        pair.guest.deepest_lag.behind <= 2,
        "{:?}",
        pair.guest.deepest_lag
    );
}
