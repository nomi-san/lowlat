//! The client's side of the signaling seam, and the one attempt it makes.
//!
//! The four calls of docs/04-signaling.md section 9, mirrored: a client makes
//! the offer, so `new_attempt` produces the credentials the application puts
//! in it, candidates come out as events for the application to relay, and
//! `begin_p2p` takes what the answer carried. Nothing here speaks to a
//! signaling service.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use lowlat_common::events::{self, Queued};
use lowlat_core::conn::Kind;
use lowlat_core::envelope::Cipher;
use lowlat_crypto::Credentials;
use lowlat_net::{Guest, Wake};

use crate::config::Config;
use crate::driver::{Telemetry, Units};

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
    ours: Credentials,
    pending: Vec<Arrival>,
    inject: Option<mpsc::Sender<Arrival>>,
    ask: Option<mpsc::Sender<Ask>>,
    thread: Option<Guest>,
}

/// How long a departure is given to reach the host before the loop stops.
/// The message is on a reliable channel and needs time to get there.
pub(crate) const LEAVE_GRACE_MS: f64 = 250.0;

/// The client. One attempt at a time, one session.
#[derive(Debug)]
pub struct Client {
    config: Config,
    attempt: Option<Attempt>,
    emit: events::Sender<Event>,
    events: Option<events::Receiver<Event>>,
    telemetry: Arc<Telemetry>,
    units: Units,
}

impl std::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attempt").field("id", &self.id).finish()
    }
}

impl Client {
    pub fn new(config: Config) -> Self {
        let (emit, events) = events::queue();
        Self {
            config,
            attempt: None,
            emit,
            events: Some(events),
            telemetry: Arc::new(Telemetry::default()),
            units: Units::new(),
        }
    }

    /// Mint the credentials the offer carries.
    ///
    /// **The media key is a capability signal.** An offer that carries one
    /// tells the host both sides can take the 256-bit cipher; the key that
    /// actually seals the session is the host's, from its answer. Leaving it
    /// out is how a client asks for the legacy cipher.
    pub fn new_attempt(&mut self, id: &str, transport: Transport) -> Result<Credentials, Error> {
        if transport == Transport::Web {
            return Err(Error::Transport);
        }
        if self.attempt.is_some() {
            return Err(Error::Busy);
        }
        let mut ours = lowlat_crypto::credentials().map_err(|_| Error::Crypto)?;
        if self.config.legacy_cipher {
            ours.aes256 = String::new();
        }
        self.attempt = Some(Attempt {
            id: id.to_string(),
            ours: ours.clone(),
            pending: Vec::new(),
            inject: None,
            ask: None,
            thread: None,
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

        let (inject, arrivals) = mpsc::channel::<Arrival>();
        for arrival in attempt.pending.drain(..) {
            let _ = inject.send(arrival);
        }
        let (ask, asked) = mpsc::channel::<Ask>();

        let args = crate::shell::Attached {
            socket,
            servers: self.config.servers.clone(),
            ours: (attempt.ours.ufrag.clone(), attempt.ours.pwd.clone()),
            theirs: (theirs.ufrag.clone(), theirs.pwd.clone()),
            material,
            cipher,
            seed,
            init: self.config.init(),
            arrivals,
            asked,
            emit: self.emit.clone(),
            telemetry: Arc::clone(&self.telemetry),
            units: self.units.clone(),
        };
        let thread = Guest::spawn(wake, move |wake, running| {
            crate::shell::run(args, wake, running)
        })
        .map_err(|_| Error::Io)?;
        attempt.inject = Some(inject);
        attempt.ask = Some(ask);
        attempt.thread = Some(thread);

        for ip in lowlat_net::host_addresses(self.config.shared_address_space) {
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
