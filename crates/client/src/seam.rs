//! The client's side of the signaling seam, and the one attempt it makes.
//!
//! The four calls of docs/04-signaling.md section 9, mirrored: a client makes
//! the offer, so `new_attempt` produces the credentials the application puts
//! in it, candidates come out as events for the application to relay, and
//! `begin_p2p` takes what the answer carried. Nothing here speaks to a
//! signaling service.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use lowlat_common::events;
use lowlat_common::spsc::Ring;
use lowlat_core::conn::Kind;
use lowlat_core::envelope::Cipher;
use lowlat_crypto::Credentials;
use lowlat_decode::software;
use lowlat_net::{Guest, Wake};

use crate::config::{Backend, Caps, Config, Decoding, FrameKind, Video};
use crate::driver::{Telemetry, Units};
use crate::event::{Arrival, Ask, DecoderStage, Error, Event, LEAVE_GRACE_MS, Peer, Transport};
use crate::frames::{Frames, Held};
use crate::input::{Input, RING_DEPTH, ReportKind, Request, Viewport};
use crate::sound::{self, Packets, Sound};
use lowlat_core::pad::{self, Product};

/// Which decoders a configuration opens, and on which device, which is the
/// platform's; the seam around them is written once.
#[cfg(target_os = "linux")]
#[path = "seam/linux.rs"]
pub(crate) mod sys;

pub use sys::Opened;

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

/// The client. One attempt at a time, one session.
#[derive(Debug)]
pub struct Client {
    attempt: Option<Attempt>,
    emit: events::Sender<Event>,
    events: Option<events::Receiver<Event>>,
    telemetry: Arc<Telemetry>,
    units: Units,
    /// The decoder opened at creation, or chosen since; none for a client
    /// without one.
    opened: Option<Opened>,
    /// What that decoder takes, which the declaration is masked with.
    caps: Caps,
    /// A decoder chosen mid-session, for the decode thread to take once the
    /// session thread has said the word.
    pending: crate::decode::Pending,
    frames: Arc<Frames>,
    /// The newest picture handed out, so the next acquire waits for newer.
    last_seq: u64,
    /// The producer's handle on the sound pool, given to each session thread.
    packets: Packets,
    /// The consumer: decoded on the application's thread, behind a lock held
    /// for the decode alone and never for the wait, so two callers serialise
    /// and each takes the next packet.
    sound: Arc<Mutex<Sound>>,
    /// Set while an attempt taken out by [`Client::detach`] is still leaving:
    /// its threads share the unit pool, the picture queue and the sound pool
    /// with whatever session comes next, so no next one begins until it is
    /// cleared.
    leaving: Arc<AtomicBool>,
}

/// An attempt taken out of its client and still leaving: everything the
/// departure and the joins need, so they run without the client and whatever
/// lock the client is behind.
#[derive(Debug)]
pub struct Leaving {
    attempt: Attempt,
    units: Units,
    frames: Arc<Frames>,
    sound: Arc<Mutex<Sound>>,
    packets: Packets,
    leaving: Arc<AtomicBool>,
}

impl Leaving {
    /// The departure and the joins: the one call that waits.
    pub fn finish(mut self) {
        let attempt = &mut self.attempt;
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
        // Release: everything above is done before a new attempt, which
        // loads the mark with Acquire, can begin.
        self.leaving.store(false, Ordering::Release);
    }
}

/// Probe the machine's own codec library: found, of a licence this build
/// loads, laid out as expected and opening a codec, or refused with the
/// stage that says which.
fn probe_software(dir: Option<&Path>) -> Result<Caps, DecoderStage> {
    let lavc = lowlat_drivers::lavc::Lavc::load(dir).map_err(|refusal| {
        use lowlat_drivers::lavc::Refusal;
        match refusal {
            Refusal::Licence => DecoderStage::Licence,
            Refusal::NoDecoder => DecoderStage::Profile,
            _ => DecoderStage::Runtime,
        }
    })?;
    Ok(software::caps(&lavc))
}

/// The directory a software decoder is asked for, from the device field:
/// none for the loader's own search.
fn software_dir(device: &str) -> Option<PathBuf> {
    (!device.is_empty()).then(|| PathBuf::from(device))
}

/// Of two stages the automatic order met, the one worth reporting: a
/// library found and refused for its licence over a node that decodes
/// nothing, that over a runtime that is absent, that over a node that did
/// not open.
fn most_telling(a: DecoderStage, b: DecoderStage) -> DecoderStage {
    let rank = |stage: DecoderStage| match stage {
        DecoderStage::Device => 0,
        DecoderStage::Runtime => 1,
        DecoderStage::Profile => 2,
        DecoderStage::Licence => 3,
        DecoderStage::Unsupported => 4,
    };
    if rank(b) > rank(a) { b } else { a }
}

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
        let (opened, caps) = sys::choose(decoding)?;
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
            pending: Arc::new(Mutex::new(None)),
            frames: Arc::new(Frames::new(decoding.ceiling(), decoding.kind)),
            last_seq: 0,
            sound: Arc::new(Mutex::new(Sound::new(packets.clone(), telemetry))),
            packets,
            leaving: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Another decoder, chosen by the application: probed here, on the
    /// caller's thread, exactly as creation probes; a kind that does not
    /// open answers with its stage and **nothing changes**. Before an attempt
    /// the choice is replaced and that is all. During a session it is one
    /// act: the declaration re-masked by the new decoder's capability and
    /// restated where it changed, the running decoder torn down, the new one
    /// opened on the decode thread, and exactly one keyframe request once
    /// it can take one. The frame kind is the queue's shape and stays the
    /// creation's: a session of the handle kind refuses this, because its
    /// device slots are bound to the device.
    pub fn set_decoder(&mut self, backend: Backend, device: &str) -> Result<(), Error> {
        if backend == Backend::None || self.frames.kind() == FrameKind::Handle {
            return Err(Error::Decoder(DecoderStage::Unsupported));
        }
        let decoding = Decoding {
            backend,
            device: device.to_string(),
            kind: self.frames.kind(),
            ceiling: self.frames.ceiling(),
        };
        let (opened, caps) = sys::choose(&decoding)?;
        let Some(opened) = opened else {
            return Err(Error::Decoder(DecoderStage::Unsupported));
        };
        self.opened = Some(opened.clone());
        self.caps = caps;
        let Some(attempt) = self.attempt.as_ref().filter(|a| a.thread.is_some()) else {
            // No session: the next one starts on the new choice.
            return Ok(());
        };
        let flags = attempt.config.video.flags(&caps);
        self.telemetry
            .declared_flags
            .store(flags, Ordering::Relaxed);
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(opened);
        }
        // The word travels behind the flags on the session thread, which
        // restates the declaration first and then tells the decode thread.
        self.request(Request::Decoder(flags));
        Ok(())
    }

    /// The decoder opened at creation, or chosen since, if there is one.
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

    /// When an arrival stamp was taken, in microseconds of the named
    /// monotonic clock: its age on the session loop's epoch, taken from a
    /// reading of that clock made at once. Zero without a running loop or
    /// the clock.
    pub fn arrived_us(&self, stamp: u32) -> u64 {
        let Some(epoch) = self.attempt.as_ref().and_then(|a| a.epoch.get().copied()) else {
            return 0;
        };
        let named = lowlat_common::clock::monotonic_us();
        if named == 0 {
            return 0;
        }
        let age = crate::driver::stamp_age_us(lowlat_common::clock::elapsed_ms(epoch), stamp);
        named.saturating_sub(u64::from(age))
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
        // An attempt still leaving is still one: its threads are running.
        // Acquire, pairing with the Release that clears the mark once they
        // are joined.
        if self.attempt.is_some() || self.leaving.load(Ordering::Acquire) {
            return Err(Error::Busy);
        }
        let mut ours = lowlat_crypto::credentials().map_err(|_| Error::Crypto)?;
        // **The attempt starts from nothing the last session left**, and no
        // thread of that session is running: there is no attempt and none is
        // leaving. Its figures are not this one's, so the status reads as
        // connecting and counts from zero; its events are not handed out under
        // this one's name; its units are not the next decoder's first.
        let telemetry = Arc::new(Telemetry::default());
        if let Ok(mut sound) = self.sound.lock() {
            *sound = Sound::new(self.packets.clone(), Arc::clone(&telemetry));
        }
        self.telemetry = telemetry;
        self.emit.clear();
        self.units.clear();
        // The departure closed the queue so no waiter was stranded; this
        // attempt's pictures are waited for again from here, the answer's
        // wait included.
        self.frames.reopen();
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
        let relay_seed = lowlat_crypto::transaction_seed().map_err(|_| Error::Crypto)?;
        // What an earlier attempt's relay left in the status is not this
        // attempt's.
        self.telemetry.relayed.store(0, Ordering::Relaxed);
        self.telemetry.path_relayed.store(false, Ordering::Relaxed);

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
            relay: attempt.config.relay.clone(),
            relay_seed,
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
            pending: Arc::clone(&self.pending),
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

        // A relay attempt offers the relayed address and nothing else, once the
        // relay has one; the session thread raises it and the readiness
        // marker after it.
        if attempt.config.relay.is_none() {
            let shared = attempt.config.shared_address_space;
            for ip in lowlat_net::host_addresses(shared) {
                self.emit.send(Event::Candidate {
                    addr: SocketAddr::new(ip, bound),
                    from_stun: false,
                    lan: true,
                });
            }
            // After the candidates, which is the order a peer expects: "that
            // is all of mine, now yours".
            self.emit.send(Event::Ready);
        }
        Ok(())
    }

    /// Leave, or abandon an attempt that never began. Emits nothing: the
    /// application caused this.
    pub fn end_connection(&mut self, id: &str) {
        if let Some(leaving) = self.detach(id) {
            leaving.finish();
        }
    }

    /// Take the attempt out to leave it, and wait for nothing. From here the
    /// client answers as having no attempt and refuses a new one until
    /// [`Leaving::finish`] has joined the threads: the departure's grace and
    /// the joins take a quarter of a second or more, and a caller holding the
    /// client behind a lock makes every other call wait that long unless it
    /// finishes outside it.
    pub fn detach(&mut self, id: &str) -> Option<Leaving> {
        let attempt = self.attempt.take_if(|attempt| attempt.id == id)?;
        // Relaxed: set and read by whoever holds the client exclusively; only
        // the clearing crosses to another thread.
        self.leaving.store(true, Ordering::Relaxed);
        Some(Leaving {
            attempt,
            units: self.units.clone(),
            frames: Arc::clone(&self.frames),
            sound: Arc::clone(&self.sound),
            packets: self.packets.clone(),
            leaving: Arc::clone(&self.leaving),
        })
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
