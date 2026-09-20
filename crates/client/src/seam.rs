//! The client's side of the signaling seam, and the one attempt it makes.
//!
//! The four calls of docs/04-signaling.md section 9, mirrored: a client makes
//! the offer, so `new_attempt` produces the credentials the application puts
//! in it, candidates come out as events for the application to relay, and
//! `begin_p2p` takes what the answer carried. Nothing here speaks to a
//! signaling service.

use std::ffi::CString;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use lowlat_common::events::{self, Queued};
use lowlat_common::spsc::Ring;
use lowlat_core::conn::Kind;
use lowlat_core::envelope::Cipher;
use lowlat_crypto::Credentials;
use lowlat_decode::{nvdec, vaapi};
use lowlat_drivers::cuda::{self, PciAddress};
use lowlat_drivers::cuvid;
use lowlat_net::{Guest, Wake};

use crate::config::{Backend, Caps, Config, Decoding, FrameKind, Video};
use crate::driver::{Telemetry, Units};
use crate::frames::{Frames, Held};
use crate::input::{Input, RING_DEPTH, ReportKind, Request, Viewport};
use crate::sound::{self, Packets, Sound};
use lowlat_core::pad::{self, Product};

/// Which pipe an attempt asks for. A client of this library uses the native
/// one; the browser's exists so a page can be a guest, and a native client
/// has no reason to be one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Bud,
    Web,
}

/// What the answer told us about the host.
#[derive(Debug, Clone)]
pub struct Peer {
    pub ufrag: String,
    pub pwd: String,
    /// The host's certificate digest: the legacy cipher's key material.
    pub fingerprint: String,
    /// The host's media key, absent from a legacy generation's answer.
    pub aes256: Option<String>,
}

/// What the application must forward to the host, or act on.
///
/// Exhaustive on purpose: the C boundary translates every variant, and one
/// added here must fail to compile there rather than fall through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A local candidate, to be sent to the host as it is found.
    Candidate {
        addr: SocketAddr,
        from_stun: bool,
        lan: bool,
    },
    /// Send the host a candidate marked `sync`, once, after every real one.
    Ready,
    /// Connectivity completed; the initialization has gone out.
    Established { addr: SocketAddr },
    /// The attempt is over, with the reason typed.
    Ended { outcome: Outcome },
    /// The host blocked this client's input, or unblocked it.
    Blocked { blocked: bool },
    /// The host ended one stream and not the session.
    StreamEnded { stream: u32, status: i32 },
    /// The host said which mode it is in.
    HostMode { mode: u32 },
    /// The host's application sent a message. Opaque here.
    UserData { id: u32, text: Vec<u8> },
    /// The host put this client into relative mode, or took it out; on the
    /// way out, where the pointer reappears, in the window's units.
    Relative { relative: bool, x: i32, y: i32 },
    /// The host's pointer changed: its picture, its hotspot or its flags.
    /// The picture, when one came or was named from the cache, travels as
    /// it did on the wire and is decoded at the boundary, on the poller's
    /// thread, into the buffer the application is lent.
    Cursor {
        /// Where the pointer reappears on the way out of relative mode, in
        /// the window's units.
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        /// In the picture's own pixels.
        hot_x: u16,
        hot_y: u16,
        hidden: bool,
        relative: bool,
        suppressed: bool,
        /// The checksum the picture is named by; zero when none is delivered.
        checksum: u32,
        /// The picture as it travelled; empty when none is delivered.
        png: Vec<u8>,
    },
    /// The host asked a pad to vibrate: the pad this client named, and the
    /// two motors as the wire carries them.
    Rumble { pad: u32, large: u8, small: u8 },
    /// What the host's device was written, for a pad this client sends as
    /// its own reports: an output report in the pad's own framing, or a
    /// feature write as the host sent it; `len` bytes of `report`.
    PadReport {
        pad: u32,
        kind: lowlat_core::pad::OutputKind,
        len: u8,
        report: [u8; lowlat_core::pad::REPORT_MAX],
    },
    /// The room as the host describes it, with this client's own number.
    /// The body is the host's application's and is not read here.
    GuestList { number: u32, body: Vec<u8> },
}

impl Queued for Event {
    fn body(&self) -> &[u8] {
        match self {
            Event::UserData { text, .. } => text,
            Event::GuestList { body, .. } => body,
            _ => &[],
        }
    }

    /// The picture is not a body: it is decoded at the boundary into the
    /// buffer the application is lent, never copied into the caller's.
    fn held(&self) -> usize {
        match self {
            Event::Cursor { png, .. } => png.len(),
            _ => 0,
        }
    }
}

/// Why an attempt finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Negotiated, and no path was found.
    ConnectivityFailed,
    /// The host stopped answering.
    PeerGone,
    /// Nothing sent has been acknowledged for the delivery deadline.
    Undeliverable,
    /// The socket or the loop failed.
    TransportFailed,
    /// The host ended the session and said why, with its own status.
    Disconnected(i32),
    /// A message arrived that this client cannot take: larger than any
    /// buffer, or unreadable. The channel cannot advance past it.
    Unreadable,
    /// No decoder can serve the stream: the device is gone, was never
    /// usable, or the stream is one it cannot decode.
    DecoderFailed,
}

/// What a seam call can refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// There is already an attempt; a client makes one at a time.
    Busy,
    /// No attempt by that name.
    UnknownAttempt,
    /// The attempt has already begun.
    AlreadyBegun,
    /// The browser pipe was asked for; a native client does not use it.
    Transport,
    /// The answer's credentials cannot key a session: no media key on an
    /// offer that carried one, or material that does not decode.
    Credentials,
    /// The platform would not supply entropy.
    Crypto,
    /// A socket or thread could not be had.
    Io,
    /// No decoder, with the stage that refused.
    Decoder(DecoderStage),
    /// The application holds as many pictures as it may.
    TooManyHeld,
    /// No session to take pictures from.
    NoSession,
    /// The buffer given holds fewer frames than the sound packet; carries
    /// how many it needs. The packet waits for the next call.
    TooSmall(usize),
    /// A pad's report is not one this path carries: the wrong length or
    /// identifier for its product, a feature report other than calibration
    /// or firmware, or a wireless checksum that does not verify.
    Report,
    /// A pad already sent as the other family: as its own reports, or as
    /// states and buttons. One family until it is unplugged.
    PadFamily,
}

/// The decoder settled at creation: which backend, on which device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opened {
    /// The open stack, on a render node.
    Vaapi(CString),
    /// The vendor's interface, on the device at an address, or the first.
    Nvdec(Option<PciAddress>),
}

/// Where building a decoder stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderStage {
    /// The runtime library is not on the machine.
    Runtime,
    /// No render node opened.
    Device,
    /// The device decodes none of the profiles a stream could use.
    Profile,
    /// A backend or a frame kind that is not built.
    Unsupported,
}

/// What reaches the loop from outside it.
pub(crate) enum Arrival {
    Candidate(SocketAddr, Kind),
    PeerReady,
}

/// What the application asks the loop to say.
pub(crate) enum Ask {
    UserData(u32, Vec<u8>),
    /// Say goodbye and stop.
    Leave,
}

/// How a pad is sent: as the sixteen-button messages, or as its own reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Standard,
    Raw,
}

/// How many pads' families are remembered; past that the rule cannot be
/// applied and the pad goes through as sent.
const PADS: usize = 8;

struct Attempt {
    id: String,
    config: Config,
    ours: Credentials,
    pending: Vec<Arrival>,
    inject: Option<mpsc::Sender<Arrival>>,
    ask: Option<mpsc::Sender<Ask>>,
    /// Input and the viewport, in order, to the session thread. Fixed depth;
    /// a full ring drops the newest and counts it, never blocks the caller.
    requests: Option<Arc<Ring<Request, RING_DEPTH>>>,
    /// The pads the application has named and which family each is sent
    /// as, so the other family is refused until an unplug.
    pads: [Option<(u32, Family)>; PADS],
    thread: Option<Guest>,
    decode: Option<(JoinHandle<()>, Arc<AtomicBool>)>,
    /// The session thread's epoch, once it is running: the clock the sound
    /// packets' arrival stamps are on.
    epoch: Arc<OnceLock<lowlat_common::clock::Time>>,
}

/// How long a departure is given to reach the host before the loop stops.
/// The message is on a reliable channel and needs time to get there.
pub(crate) const LEAVE_GRACE_MS: f64 = 250.0;

/// The client. One attempt at a time, one session.
#[derive(Debug)]
pub struct Client {
    attempt: Option<Attempt>,
    emit: events::Sender<Event>,
    events: Option<events::Receiver<Event>>,
    telemetry: Arc<Telemetry>,
    units: Units,
    /// The decoder opened at creation; none for a client without one.
    opened: Option<Opened>,
    /// What that decoder takes, which the declaration is masked with.
    caps: Caps,
    frames: Arc<Frames>,
    /// The newest picture handed out, so the next acquire waits for newer.
    last_seq: u64,
    /// The producer's handle on the sound pool, given to each session thread.
    packets: Packets,
    /// The consumer: decoded on the application's thread, behind a lock held
    /// for the decode alone and never for the wait, so two callers serialise
    /// and each takes the next packet.
    sound: Arc<Mutex<Sound>>,
}

/// Which stage a probe's refusal names.
fn stage_of(error: &vaapi::Error) -> DecoderStage {
    match error {
        vaapi::Error::Runtime(
            vaapi::RuntimeError::Unavailable | vaapi::RuntimeError::MissingSymbol,
        ) => DecoderStage::Runtime,
        vaapi::Error::NoProfile => DecoderStage::Profile,
        _ => DecoderStage::Device,
    }
}

/// The card behind a render node, as the compute runtime addresses it:
/// the node's device link in the kernel's tree names the bus address.
pub(crate) fn address_of(node: &str) -> Option<PciAddress> {
    let name = std::path::Path::new(node).file_name()?.to_str()?;
    let link = std::fs::read_link(format!("/sys/class/drm/{name}/device")).ok()?;
    PciAddress::parse(link.file_name()?.to_str()?)
}

/// The render node on a card, by its bus address: the inverse of
/// [`address_of`], over the nodes this crate looks at.
pub(crate) fn node_of(address: PciAddress) -> Option<&'static str> {
    RENDER_NODES
        .iter()
        .copied()
        .find(|node| address_of(node) == Some(address))
}

/// Probe the vendor's interface on `address`, or on the first device: the
/// runtimes loaded, the context made current here, a real decoder built per
/// combination.
fn probe_nvdec(address: Option<PciAddress>) -> Result<Caps, DecoderStage> {
    let cuda = cuda::Cuda::load().map_err(|_| DecoderStage::Runtime)?;
    let device = match address {
        Some(address) => cuda.device_at(address),
        None => cuda.any_device(),
    }
    .map_err(|_| DecoderStage::Device)?;
    let context = cuda
        .retain_primary(&device)
        .map_err(|_| DecoderStage::Device)?;
    context.make_current().map_err(|_| DecoderStage::Device)?;
    let loaded = cuvid::Cuvid::load().map_err(|_| DecoderStage::Runtime)?;
    let caps = nvdec::caps(&loaded);
    // The application's thread, left as it was found.
    let _ = context.release_current();
    if caps.any() {
        Ok(caps)
    } else {
        Err(DecoderStage::Profile)
    }
}

/// Where the first render node that decodes is looked for.
pub(crate) const RENDER_NODES: [&str; 8] = [
    "/dev/dri/renderD128",
    "/dev/dri/renderD129",
    "/dev/dri/renderD130",
    "/dev/dri/renderD131",
    "/dev/dri/renderD132",
    "/dev/dri/renderD133",
    "/dev/dri/renderD134",
    "/dev/dri/renderD135",
];

impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt").field("id", &self.id).finish()
    }
}

impl Client {
    /// Create a client, opening its decoder's device once to see that it
    /// decodes: a machine without one is refused here, with the stage named,
    /// rather than after it has connected.
    pub fn new(decoding: &Decoding) -> Result<Self, Error> {
        let (opened, caps) = match (decoding.kind, decoding.backend) {
            // Only the vendor backend exports a handle, so asking for one
            // settles the choice: the vendor's on any device, or nothing.
            (FrameKind::Handle, Backend::Vaapi) => {
                return Err(Error::Decoder(DecoderStage::Unsupported));
            }
            (FrameKind::Handle, Backend::Auto) => {
                let caps =
                    probe_nvdec(None).map_err(|_| Error::Decoder(DecoderStage::Unsupported))?;
                (Some(Opened::Nvdec(None)), caps)
            }
            (_, Backend::None) => (None, Caps::default()),
            (_, Backend::Nvdec) => {
                // The device is named as a render node, as for the open
                // stack; the card behind it is what the runtime takes.
                let address = if decoding.device.is_empty() {
                    None
                } else {
                    Some(address_of(&decoding.device).ok_or(Error::Decoder(DecoderStage::Device))?)
                };
                let caps = probe_nvdec(address).map_err(Error::Decoder)?;
                (Some(Opened::Nvdec(address)), caps)
            }
            (_, Backend::Vaapi | Backend::Auto) => {
                if decoding.device.is_empty() {
                    let mut found = None;
                    let mut last = DecoderStage::Device;
                    for candidate in RENDER_NODES {
                        let Ok(path) = CString::new(candidate) else {
                            continue;
                        };
                        match vaapi::probe(&path) {
                            Ok(caps) if caps.any() => {
                                found = Some((path, caps));
                                break;
                            }
                            Ok(_) => last = DecoderStage::Profile,
                            Err(e) => {
                                if stage_of(&e) == DecoderStage::Runtime {
                                    last = DecoderStage::Runtime;
                                }
                            }
                        }
                    }
                    match found {
                        Some((path, caps)) => (Some(Opened::Vaapi(path)), caps),
                        // Nothing decodes through the open stack: the
                        // vendor's interface on any device, if there is one.
                        None if decoding.backend == Backend::Auto => {
                            let caps = probe_nvdec(None).map_err(|_| Error::Decoder(last))?;
                            (Some(Opened::Nvdec(None)), caps)
                        }
                        None => return Err(Error::Decoder(last)),
                    }
                } else {
                    let path = CString::new(decoding.device.as_str())
                        .map_err(|_| Error::Decoder(DecoderStage::Device))?;
                    let caps = vaapi::probe(&path).map_err(|e| Error::Decoder(stage_of(&e)))?;
                    if !caps.any() {
                        return Err(Error::Decoder(DecoderStage::Profile));
                    }
                    (Some(Opened::Vaapi(path)), caps)
                }
            }
        };
        let (emit, events) = events::queue();
        let telemetry = Arc::new(Telemetry::default());
        let packets = Packets::new();
        Ok(Self {
            attempt: None,
            emit,
            events: Some(events),
            telemetry: Arc::clone(&telemetry),
            units: Units::new(),
            opened,
            caps,
            frames: Arc::new(Frames::new(decoding.ceiling(), decoding.kind)),
            last_seq: 0,
            sound: Arc::new(Mutex::new(Sound::new(packets.clone(), telemetry))),
            packets,
        })
    }

    /// The decoder opened at creation, if there is one.
    pub fn opened(&self) -> Option<&Opened> {
        self.opened.as_ref()
    }

    /// What the decoder takes.
    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// The picture queue.
    pub fn frames(&self) -> &Arc<Frames> {
        &self.frames
    }

    /// The newest picture, newer than the last one handed out, waiting up
    /// to `timeout`. `Ok(None)` when none came in time.
    pub fn acquire_frame(&mut self, timeout: Duration) -> Result<Option<Held>, Error> {
        if self.attempt.is_none() {
            return Err(Error::NoSession);
        }
        match self.frames.acquire(self.last_seq, timeout) {
            Ok(Some(held)) => {
                self.last_seq = held.seq;
                Ok(Some(held))
            }
            Ok(None) => Ok(None),
            Err(crate::frames::TooManyHeld) => Err(Error::TooManyHeld),
        }
    }

    /// The sequence of the newest picture handed out, for a caller that
    /// waits on the queue itself and reports back what it took.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn set_last_seq(&mut self, seq: u64) {
        self.last_seq = self.last_seq.max(seq);
    }

    /// The application is done with a picture.
    pub fn release_frame(&mut self, index: usize) {
        self.frames.release(index);
    }

    /// What a caller waits on for sound outside the handle's lock: the
    /// consumer, the pool's handle, and the session thread's epoch for the
    /// age. `None` without a session.
    pub fn sound(&self) -> Option<Listening> {
        let attempt = self
            .attempt
            .as_ref()
            .filter(|attempt| attempt.thread.is_some())?;
        Some(Listening {
            sound: Arc::clone(&self.sound),
            packets: self.packets.clone(),
            epoch: Arc::clone(&attempt.epoch),
        })
    }

    /// Sound packets handed over by the session thread and not yet taken.
    pub fn sound_queued(&self) -> usize {
        self.packets.queued()
    }
}

/// The handles an `acquire_audio` needs, taken from the seam under its lock
/// and used outside it.
#[derive(Debug, Clone)]
pub struct Listening {
    sound: Arc<Mutex<Sound>>,
    packets: Packets,
    epoch: Arc<OnceLock<lowlat_common::clock::Time>>,
}

impl Listening {
    /// The next sound packet into `out`, waiting up to `timeout`. The lock
    /// on the consumer is taken for the decode and not for the wait.
    pub fn acquire(
        &self,
        timeout: Duration,
        out: &mut [i16],
    ) -> Result<Option<sound::Acquired>, Error> {
        let began = lowlat_common::clock::Time::now();
        let mut remaining = timeout;
        loop {
            let now_ms = self
                .epoch
                .get()
                .map_or(0.0, |base| lowlat_common::clock::elapsed_ms(*base));
            let taken = {
                let mut guard = self.sound.lock().map_err(|_| Error::NoSession)?;
                guard.acquire(now_ms, Duration::ZERO, out)
            };
            match taken {
                Ok(Some(acquired)) => return Ok(Some(acquired)),
                Ok(None) => {}
                Err(sound::TooSmall(frames)) => return Err(Error::TooSmall(frames)),
            }
            if remaining.is_zero() {
                return Ok(None);
            }
            self.packets.wait(remaining);
            let elapsed = lowlat_common::clock::elapsed_ms(began);
            let total = timeout.as_secs_f64() * 1000.0;
            remaining = if elapsed >= total {
                Duration::ZERO
            } else {
                Duration::from_secs_f64((total - elapsed) / 1000.0)
            };
        }
    }
}

impl Client {
    /// Mint the credentials the offer carries.
    ///
    /// **The media key is a capability signal.** An offer that carries one
    /// tells the host both sides can take the 256-bit cipher; the key that
    /// actually seals the session is the host's, from its answer. Leaving it
    /// out is how a client asks for the legacy cipher.
    pub fn new_attempt(
        &mut self,
        id: &str,
        config: Config,
        transport: Transport,
    ) -> Result<Credentials, Error> {
        if transport == Transport::Web {
            return Err(Error::Transport);
        }
        if self.attempt.is_some() {
            return Err(Error::Busy);
        }
        let mut ours = lowlat_crypto::credentials().map_err(|_| Error::Crypto)?;
        if config.legacy_cipher {
            ours.aes256 = String::new();
        }
        self.attempt = Some(Attempt {
            id: id.to_string(),
            config,
            ours: ours.clone(),
            pending: Vec::new(),
            inject: None,
            ask: None,
            requests: None,
            pads: [None; PADS],
            thread: None,
            decode: None,
            epoch: Arc::new(OnceLock::new()),
        });
        Ok(ours)
    }

    /// A candidate arrived from the host. `sync` marks a readiness signal,
    /// not an address; unknown attempts are a no-op.
    pub fn add_candidate(&mut self, id: &str, addr: SocketAddr, sync: bool, kind: Kind) {
        let Some(attempt) = self.attempt.as_mut().filter(|attempt| attempt.id == id) else {
            return;
        };
        let arrival = if sync {
            Arrival::PeerReady
        } else {
            Arrival::Candidate(addr, kind)
        };
        match (&attempt.inject, &attempt.thread) {
            (Some(inject), Some(thread)) => {
                if inject.send(arrival).is_ok() {
                    let _ = thread.wake_handle().notify();
                }
            }
            _ => attempt.pending.push(arrival),
        }
    }

    /// The answer arrived. Bind, start connectivity, and begin trickling
    /// candidates as events.
    pub fn begin_p2p(&mut self, id: &str, theirs: &Peer) -> Result<(), Error> {
        let attempt = self
            .attempt
            .as_mut()
            .filter(|attempt| attempt.id == id)
            .ok_or(Error::UnknownAttempt)?;
        if attempt.thread.is_some() {
            return Err(Error::AlreadyBegun);
        }

        // **Keyed from the answer's block.** The host seals both directions
        // with its own material: its media key when both ends offered one,
        // its certificate digest under the legacy cipher when either did not.
        // The cipher is decided by presence, never by a string's length.
        let (cipher, source) = match (&theirs.aes256, attempt.ours.aes256.is_empty()) {
            (Some(key), false) if !key.is_empty() => (Cipher::Aes256, key.as_str()),
            _ => (Cipher::Aes128, theirs.fingerprint.as_str()),
        };
        let key_len = cipher.key_len();
        let (key, prefix) =
            lowlat_crypto::key_material(source, key_len).map_err(|_| Error::Credentials)?;
        let mut material = [0u8; 36];
        material
            .get_mut(..key_len)
            .ok_or(Error::Credentials)?
            .copy_from_slice(key.get(..key_len).ok_or(Error::Credentials)?);
        material
            .get_mut(key_len..key_len + prefix.len())
            .ok_or(Error::Credentials)?
            .copy_from_slice(&prefix);
        if cipher == Cipher::Aes128 {
            lowlat_common::log_info!(
                "client: attempt={} takes the legacy cipher, the exchange carried no media key",
                id
            );
        }
        let seed = lowlat_crypto::transaction_seed().map_err(|_| Error::Crypto)?;

        let socket = lowlat_net::Socket::open_or_any_port(0).map_err(|_| Error::Io)?;
        let bound = socket.local_addr().map_err(|_| Error::Io)?.port();
        let wake = Wake::new().map_err(|_| Error::Io)?;
        let shell_wake = wake.handle().map_err(|_| Error::Io)?;

        let (inject, arrivals) = mpsc::channel::<Arrival>();
        for arrival in attempt.pending.drain(..) {
            let _ = inject.send(arrival);
        }
        let (ask, asked) = mpsc::channel::<Ask>();
        let requests: Arc<Ring<Request, RING_DEPTH>> = Arc::new(Ring::new());

        let args = crate::shell::Attached {
            socket,
            servers: attempt.config.servers.clone(),
            ours: (attempt.ours.ufrag.clone(), attempt.ours.pwd.clone()),
            theirs: (theirs.ufrag.clone(), theirs.pwd.clone()),
            material,
            cipher,
            seed,
            init: {
                let init = attempt.config.init(&self.caps);
                self.telemetry
                    .asked_flags
                    .store(attempt.config.video.asked(), Ordering::Relaxed);
                self.telemetry
                    .declared_flags
                    .store(init.flags, Ordering::Relaxed);
                init
            },
            arrivals,
            asked,
            requests: Arc::clone(&requests),
            emit: self.emit.clone(),
            telemetry: Arc::clone(&self.telemetry),
            units: self.units.clone(),
            packets: self.packets.clone(),
            epoch: Arc::clone(&attempt.epoch),
        };
        let thread = Guest::spawn(wake, move |wake, running| {
            crate::shell::run(args, wake, running)
        })
        .map_err(|_| Error::Io)?;
        attempt.inject = Some(inject);
        attempt.ask = Some(ask);
        attempt.requests = Some(requests);
        attempt.thread = Some(thread);

        // The decode thread, beside it. It opens the device itself and
        // holds it for its life. Without a decoder the units are dropped
        // where they land, so the pool never fills.
        let stopping = Arc::new(AtomicBool::new(false));
        let decode_args = crate::decode::Attached {
            opened: self.opened.clone(),
            units: self.units.clone(),
            frames: Arc::clone(&self.frames),
            telemetry: Arc::clone(&self.telemetry),
            emit: self.emit.clone(),
            shell: shell_wake,
            stopping: Arc::clone(&stopping),
        };
        let decode = std::thread::Builder::new()
            .name("lowlat-decode".into())
            .spawn(move || crate::decode::run(decode_args))
            .map_err(|_| Error::Io)?;
        attempt.decode = Some((decode, stopping));
        self.last_seq = 0;

        let shared = attempt.config.shared_address_space;
        for ip in lowlat_net::host_addresses(shared) {
            self.emit.send(Event::Candidate {
                addr: SocketAddr::new(ip, bound),
                from_stun: false,
                lan: true,
            });
        }
        // After the candidates, which is the order a peer expects: "that is
        // all of mine, now yours".
        self.emit.send(Event::Ready);
        Ok(())
    }

    /// Leave, or abandon an attempt that never began. Emits nothing: the
    /// application caused this.
    pub fn end_connection(&mut self, id: &str) {
        let Some(mut attempt) = self.attempt.take_if(|attempt| attempt.id == id) else {
            return;
        };
        if let (Some(ask), Some(thread)) = (attempt.ask.as_ref(), attempt.thread.as_mut()) {
            // A clean departure says so on the control channel, and the loop
            // gives the message its grace before it stops itself.
            if ask.send(Ask::Leave).is_ok() {
                let _ = thread.wake_handle().notify();
                let began = lowlat_common::clock::Time::now();
                while thread.alive()
                    && lowlat_common::clock::elapsed_ms(began) < LEAVE_GRACE_MS * 2.0
                {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            thread.stop();
        }
        if let Some((decode, stopping)) = attempt.decode.take() {
            // Teardown wakes before it joins: the flag, then the word the
            // thread waits on, then the picture queue's waiters.
            stopping.store(true, Ordering::Release);
            self.units.wake();
            self.frames.close();
            let _ = decode.join();
        }
        // What the session left in the sound pool is its own; the next
        // session's first packet is not to wait behind it.
        if let Ok(mut sound) = self.sound.lock() {
            sound.clear();
        }
        self.packets.wake();
    }

    /// Hand the session thread one request and wake it.
    ///
    /// **A full ring drops the request and counts it.** The caller is the
    /// application's thread and is never blocked; a ring that fills is a
    /// session thread that is not running.
    fn request(&mut self, request: Request) -> bool {
        let Some(attempt) = self.attempt.as_ref() else {
            return false;
        };
        let (Some(requests), Some(thread)) = (attempt.requests.as_ref(), attempt.thread.as_ref())
        else {
            return false;
        };
        if requests.push(request).is_err() {
            self.telemetry.input_dropped.fetch_add(1, Ordering::Relaxed);
        } else {
            // Every push wakes the loop. Waking only when the ring was empty
            // loses the wake that lands between the consumer's last pop and
            // its sleep, and the report then waits for the next timer.
            let _ = thread.wake_handle().notify();
        }
        true
    }

    /// A new preference for the picture, mid-session. The declaration is
    /// masked by capability as at the attempt, restated to the host, and the
    /// decoder torn down with a keyframe asked for, so the next one builds
    /// for whatever the host now sends. Refused without an attempt to apply
    /// it to.
    pub fn set_video(&mut self, video: Video) -> Result<(), Error> {
        let Some(attempt) = self.attempt.as_mut() else {
            return Err(Error::UnknownAttempt);
        };
        attempt.config.video = video;
        let flags = video.flags(&self.caps);
        self.telemetry
            .asked_flags
            .store(video.asked(), Ordering::Relaxed);
        self.telemetry
            .declared_flags
            .store(flags, Ordering::Relaxed);
        self.request(Request::Video(flags));
        Ok(())
    }

    /// Where the application drew the picture, in the units its positions
    /// use. False if there is no session.
    pub fn set_viewport(&mut self, viewport: Viewport) -> bool {
        self.request(Request::Viewport(viewport))
    }

    /// One input report. `Err(NoSession)` without one; a full ring drops it
    /// and counts it rather than saying so here. A state, button or axis for
    /// a pad sent as its own reports is refused (`PadFamily`); an unplug
    /// forgets the pad's family.
    pub fn send_input(&mut self, input: Input) -> Result<(), Error> {
        let pad = match input {
            Input::PadState { pad, .. }
            | Input::PadButton { pad, .. }
            | Input::PadAxis { pad, .. } => Some(pad),
            Input::PadUnplug { pad } => {
                self.forget_pad(pad);
                None
            }
            _ => None,
        };
        if let Some(pad) = pad {
            self.claim_pad(pad, Family::Standard)?;
        }
        if self.request(Request::Input(input)) {
            Ok(())
        } else {
            Err(Error::NoSession)
        }
    }

    /// A DualShock 4's or a DualSense's own report, as the pad delivered it
    /// (docs/10-client.md section 8): normalised to the USB form here, on the
    /// application's thread, and handed to the session thread with the
    /// product and the transport. `Report` for one this path does not carry,
    /// `PadFamily` for a pad already sent as states.
    pub fn send_pad_report(
        &mut self,
        pad: u32,
        product: Product,
        kind: ReportKind,
        report: &[u8],
    ) -> Result<(), Error> {
        let mut normalised = [0u8; pad::INPUT_LEN];
        let (transport, len) = match kind {
            ReportKind::Input => {
                let transport = pad::normalize_input(product, report, &mut normalised)
                    .map_err(|_| Error::Report)?;
                (transport, pad::INPUT_LEN)
            }
            ReportKind::Feature => {
                let mut feature = [0u8; pad::FEATURE_MAX];
                let (_, len) = pad::normalize_feature(product, report, &mut feature)
                    .map_err(|_| Error::Report)?;
                if let (Some(dst), Some(src)) = (normalised.get_mut(..len), feature.get(..len)) {
                    dst.copy_from_slice(src);
                }
                // A feature report says nothing about the transport: a
                // wireless pad's checksum was stripped above, and the mapper
                // keeps what the input reports said.
                (pad::Transport::Usb, len)
            }
        };
        self.claim_pad(pad, Family::Raw)?;
        let input = Input::PadReport {
            pad,
            product,
            kind,
            transport,
            len: u8::try_from(len).unwrap_or(u8::MAX),
            report: normalised,
        };
        if self.request(Request::Input(input)) {
            Ok(())
        } else {
            Err(Error::NoSession)
        }
    }

    /// Record which family a pad is sent as, refusing the other.
    fn claim_pad(&mut self, pad: u32, family: Family) -> Result<(), Error> {
        let Some(attempt) = self.attempt.as_mut() else {
            return Err(Error::NoSession);
        };
        if let Some((_, known)) = attempt.pads.iter().flatten().find(|(id, _)| *id == pad) {
            return if *known == family {
                Ok(())
            } else {
                Err(Error::PadFamily)
            };
        }
        if let Some(slot) = attempt.pads.iter_mut().find(|s| s.is_none()) {
            *slot = Some((pad, family));
        }
        Ok(())
    }

    fn forget_pad(&mut self, pad: u32) {
        if let Some(attempt) = self.attempt.as_mut() {
            for slot in attempt.pads.iter_mut() {
                if slot.is_some_and(|(id, _)| id == pad) {
                    *slot = None;
                }
            }
        }
    }

    /// Send the host's application a message. False if there is no session.
    pub fn send_user_data(&mut self, id: u32, text: &[u8]) -> bool {
        let Some(attempt) = self.attempt.as_ref() else {
            return false;
        };
        match (attempt.ask.as_ref(), attempt.thread.as_ref()) {
            (Some(ask), Some(thread)) => {
                ask.send(Ask::UserData(id, text.to_vec())).is_ok()
                    && thread.wake_handle().notify().is_ok()
            }
            _ => false,
        }
    }

    /// What the loop reports about itself.
    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    /// Where received access units come out, for the consumer of them.
    pub fn units(&self) -> Units {
        self.units.clone()
    }

    pub fn poll_event(&mut self) -> Option<events::Received<Event>> {
        self.events.as_ref().and_then(events::Receiver::try_recv)
    }

    /// Give the application the queue itself, once, so it can wait on it.
    pub fn take_events(&mut self) -> Option<events::Receiver<Event>> {
        self.events.take()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(id) = self.attempt.as_ref().map(|attempt| attempt.id.clone()) {
            self.end_connection(&id);
        }
    }
}
