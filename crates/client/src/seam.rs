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
use lowlat_decode::vaapi;
use lowlat_net::{Guest, Wake};

use crate::config::{Backend, Config, Decoding, FrameKind};
use crate::driver::{Telemetry, Units};
use crate::frames::{Frames, Held};
use crate::input::{Input, RING_DEPTH, Request, Viewport};
use crate::sound::{self, Packets, Sound};

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
}

impl Queued for Event {
    fn body(&self) -> &[u8] {
        match self {
            Event::UserData { text, .. } => text,
            _ => &[],
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
    /// The render node the decoder opens, settled at creation; none for a
    /// client without a decoder.
    node: Option<CString>,
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

/// Where the first render node that decodes is looked for.
const RENDER_NODES: [&str; 8] = [
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
        if decoding.backend == Backend::Nvdec || decoding.kind == FrameKind::Handle {
            return Err(Error::Decoder(DecoderStage::Unsupported));
        }
        let node = if decoding.backend == Backend::None {
            None
        } else if decoding.device.is_empty() {
            let mut found = None;
            let mut last = DecoderStage::Device;
            for candidate in RENDER_NODES {
                let Ok(path) = CString::new(candidate) else {
                    continue;
                };
                match vaapi::probe(&path) {
                    Ok(caps) if caps.any() => {
                        found = Some(path);
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
            Some(found.ok_or(Error::Decoder(last))?)
        } else {
            let path = CString::new(decoding.device.as_str())
                .map_err(|_| Error::Decoder(DecoderStage::Device))?;
            let caps = vaapi::probe(&path).map_err(|e| Error::Decoder(stage_of(&e)))?;
            if !caps.any() {
                return Err(Error::Decoder(DecoderStage::Profile));
            }
            Some(path)
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
            node,
            frames: Arc::new(Frames::new(decoding.ceiling())),
            last_seq: 0,
            sound: Arc::new(Mutex::new(Sound::new(packets.clone(), telemetry))),
            packets,
        })
    }

    /// The render node the decoder opens, if there is a decoder.
    pub fn node(&self) -> Option<&CString> {
        self.node.as_ref()
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
            init: attempt.config.init(),
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
            node: self.node.clone(),
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

    /// Where the application drew the picture, in the units its positions
    /// use. False if there is no session.
    pub fn set_viewport(&mut self, viewport: Viewport) -> bool {
        self.request(Request::Viewport(viewport))
    }

    /// One input report. False if there is no session; a full ring drops it
    /// and counts it rather than saying so here.
    pub fn send_input(&mut self, input: Input) -> bool {
        self.request(Request::Input(input))
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
