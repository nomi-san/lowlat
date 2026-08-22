//! The admission seam: four calls and an event queue.
//!
//! This is the whole of what a signaling implementation has to drive, and it is
//! deliberately poll based rather than callback based. A callback runs on our
//! thread, so an application that blocks in one stalls a media loop, and every
//! integration has to reason about which thread it is on. A queue the caller
//! drains moves that decision to the caller and keeps our threads ours.
//!
//! **Admission is the application's decision.** Nothing here applies policy
//! beyond capacity: no allow list, no interactive approval, no identity check.
//! Those live above the seam, where the information is.
//!
//! One socket per guest, bound by walking from the configured base. Concurrent
//! guests therefore occupy consecutive ports, because the socket punched for an
//! attempt becomes that guest's media socket for the whole session rather than
//! being handed back.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::mpsc;

use lowlat_core::channel::{RecvRing, SlotMeta};
use lowlat_core::conn::{Conn, Credentials};
use lowlat_core::control::{self, CONTROL_CHANNEL, status};
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::{Cipher, ENVELOPE_LEN, Envelope};
use lowlat_core::packet::HEADER_LEN;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::{DELIVERY_DEADLINE_MS, Session};
use lowlat_core::video::Rotation;
use lowlat_net::{Guest, Shell, Socket, Wake};

use crate::session::{Negotiation, State};
use crate::stream::{SeatHold, Seats, Stream};
use crate::video::Packetiser;
use lowlat_inject::event::{Extents, Injector, Place};
use lowlat_inject::uinput::Devices;

/// Video, stream 0. See docs/01-protocol.md section 6.
const VIDEO_CHANNEL: u8 = 1;

/// How long a session stays up after the peer has been told it is over.
///
/// **The reason rides a reliable channel**, so it is retransmitted if the
/// first attempt is lost. Tearing the session down on the pass that queued it
/// throws the message away with the ring it is sitting in, and the peer is
/// left to discover the disconnection from its own liveness deadline, which is
/// exactly what saying why was meant to avoid.
const KICK_GRACE_MS: f64 = 250.0;

/// What one fragment carries, and therefore what one ring slot holds.
///
/// **Derived from the datagram floor, not from the path.** The size a peer
/// accepts is not negotiated and cannot be, so the floor is the only size
/// entitled to be emitted before a probe has justified anything larger. A
/// slot wide enough for a bigger datagram is not free headroom: the slot
/// width *is* the fragment width, so widening it puts oversized datagrams on
/// a path nothing has measured, and a peer that cannot take one discards the
/// whole thing silently.
const BODY: usize = lowlat_core::DEFAULT_DATAGRAM - ENVELOPE_LEN - HEADER_LEN;
const SLOT: usize = BODY;

/// Ring depths, per channel and direction.
///
/// **Sized from the largest frame the stream can produce**, not from control
/// traffic. The send window may never exceed the peer's ring depth
/// (docs/01-protocol.md section 7), which is also where the delivery gate's
/// top ceiling comes from, so the video ring is exactly that depth: anything
/// less would refuse a frame the gate had already admitted.
const VIDEO_SEND_SLOTS: usize = 4000;
/// Control we send is a handful of small messages per second.
const CONTROL_SEND_SLOTS: usize = 256;
/// Control we receive has to hold the peer's in-flight window plus whatever
/// single message is longest, and a long one is a real shape: a message of
/// several hundred fragments is recorded on this protocol.
const CONTROL_RECV_SLOTS: usize = 1024;

/// The longest inbound control message that will be taken.
///
/// **A message longer than this wedges the channel**, because a take that
/// does not fit does not consume it and the next pass reads the same one
/// forever. So the buffer is generous against what a peer actually sends --
/// declarations, input batches, and small configuration bodies -- and a
/// message past it ends the attempt rather than stalling it silently.
const MAX_INBOUND: usize = 64 * 1024;

/// How often the loop reports what it has gathered, in passes. Cheap, and only
/// reads state the loop already owns.
const REPORT_EVERY: u32 = 16;

/// The buttons the live-run probe watches for, as the whole-pad message packs
/// them: the two face buttons nearest the thumb.
const PROBE_CHORD: u16 = 0x1000 | 0x2000;

/// How long the probe asks for.
const PROBE_MS: f64 = 500.0;

/// How often a streaming guest says how it is doing, in milliseconds.
const PROGRESS_MS: f64 = 2000.0;

/// A 256-bit key and the four-byte nonce prefix that follows it.
const MATERIAL_LEN: usize = 36;

/// What the application must forward to the peer, or act on.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// A local candidate, to be sent to the peer as it is found. Trickled, not
    /// batched: batching adds the slowest interface to every setup.
    Candidate {
        attempt: String,
        addr: SocketAddr,
        from_stun: bool,
    },
    /// Send the peer a candidate marked `sync`, once.
    ///
    /// It is a readiness marker rather than an address, and **a peer may
    /// withhold every real candidate until it arrives**, so a host that never
    /// emits one negotiates with a peer that never offers anything to check.
    /// Whatever address rides along is ignored by the receiver.
    Ready { attempt: String },
    /// Connectivity completed and media can flow.
    Established { attempt: String, addr: SocketAddr },
    /// The attempt is over, with the reason typed rather than a timeout.
    Ended { attempt: String, outcome: Outcome },
    /// A guest sent its application a message ([01 §11.1](01-protocol.md)).
    ///
    /// **Nothing here reads the body.** The sub-identifier and the text are an
    /// application's own protocol, and a host that interpreted either would be
    /// inventing one on its behalf (docs/05-host.md section 5).
    UserData { guest: u32, id: u32, text: Vec<u8> },
}

/// Why an attempt finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// Negotiated, but no path was found. The only outcome that justifies
    /// escalating to a relay.
    ConnectivityFailed,
    /// The peer stopped answering. An established session learns this from the
    /// media path rather than from signaling, because a peer that closes a
    /// session it was using does not withdraw its offer.
    PeerGone,
    /// Nothing sent to the peer has been acknowledged for the delivery
    /// deadline, while there was something outstanding the whole time.
    ///
    /// **Distinct from [`Outcome::PeerGone`] because the peer is not
    /// necessarily gone.** It may still be acknowledging on the cadence and
    /// have stopped receiving, which satisfies every deadline that watches the
    /// inbound direction while the whole send window is retransmitted for as
    /// long as the session is allowed to last. The two are told apart only by
    /// whether a window moves, and a live run needs to know which it saw.
    Undeliverable,
    /// The peer said it was leaving, on the control channel.
    ///
    /// **Distinct from [`Outcome::PeerGone`], and it is the difference between
    /// a seat freed now and a seat freed two minutes from now.** Signaling
    /// carries nothing when a peer closes a session it was using, so without
    /// this the only thing that notices is the media path's liveness deadline,
    /// and the guest's port and its share of the bitrate budget stay spent
    /// until then. Repeated test connections exhaust capacity that way.
    PeerLeft,
    /// Connected, then never said what it could decode.
    ///
    /// Distinct from [`Outcome::PeerGone`] on purpose: a peer that reached us
    /// and then sat silent is either not speaking this protocol or died in the
    /// handshake, and reporting that as a peer that went away sends the next
    /// diagnosis to the network rather than to the negotiation.
    NeverDeclared,
    /// The socket could not be driven any further.
    ///
    /// **Rare, and it is about this host rather than about the peer.** A
    /// datagram the path refuses is dropped like loss and never reaches here;
    /// what does is a socket that has stopped working, which no retry answers.
    TransportFailed,
    /// The control stream could not be read any further.
    ///
    /// A message that cannot be taken does not advance the channel, so this is
    /// terminal rather than a frame to skip.
    ControlStalled,
    /// The host ended it, and told the peer why.
    ///
    /// **The only outcome the peer is given a reason for.** Every other one
    /// here is something that happened to the session; this is a decision, and
    /// a decision the peer is not told about is indistinguishable from a host
    /// that stopped working.
    Kicked(i32),
}

/// What the offer told us about the peer.
#[derive(Debug, Clone)]
pub struct Peer {
    pub ufrag: String,
    pub pwd: String,
    /// Media key material. Retained because the answer's key is ours, not
    /// theirs; this is kept for the legacy path and for logging its absence.
    pub aes256: Option<String>,
    /// What this peer may drive, as signaling reported it.
    pub permissions: lowlat_inject::event::Permissions,
    /// Whether this peer owns the machine, which decides only one thing: it
    /// takes the pointer from another guest rather than waiting for it
    /// ([`crate::floor`]).
    pub owner: bool,
}

/// Credentials the answer carries back.
///
/// **Generated at approval, not at registration.** They are bound to the socket
/// that was just opened for this attempt, so generating them earlier binds them
/// to nothing and generating them per registration leaks state for attempts
/// that are never approved.
#[derive(Debug, Clone)]
pub struct HostCredentials {
    pub ufrag: String,
    pub pwd: String,
    pub fingerprint: String,
    pub aes256: String,
    /// The port this guest was actually bound to, which is not necessarily the
    /// configured one. Advertising the configured port when the bind walked
    /// produces a peer that answers checks and never establishes.
    pub port: u16,
}

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No attempt with that identifier. A race with teardown rather than a
    /// fault, and callers treat it as one.
    UnknownAttempt,
    /// Already at the configured guest limit.
    AtCapacity,
    /// The attempt has already been approved.
    AlreadyBegun,
    /// Withdrawn before it was registered, so it is over before it began.
    Withdrawn,
    /// A socket could not be opened, or a thread could not be spawned.
    Io,
    /// Entropy or key material.
    Crypto,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::UnknownAttempt => "no such attempt",
            Self::AtCapacity => "at the configured guest limit",
            Self::AlreadyBegun => "attempt already approved",
            Self::Withdrawn => "attempt was withdrawn before it was registered",
            Self::Io => "socket or thread could not be created",
            Self::Crypto => "credentials could not be produced",
        };
        f.write_str(text)
    }
}

impl std::error::Error for Error {}

/// How the host is configured.
#[derive(Debug, Clone)]
pub struct Config {
    /// The base every guest's bind walks from.
    pub base_port: u16,
    /// Advertised capacity, and the only policy this seam applies.
    pub max_guests: usize,
    /// Reflexive servers, consulted for our own mapped address.
    pub servers: Vec<SocketAddr>,
    /// The media stream every guest is served from, or `None` for a host that
    /// admits guests and sends them nothing. Absent is what the seam's own
    /// tests use, so they need neither a thread nor a device.
    pub stream: Option<crate::stream::Config>,
    /// Whether one guest at a time may drive the pointer
    /// ([`crate::floor`]). Off by default: one person driving is a
    /// configuration, not a law.
    pub exclusive_pointer: bool,
    /// **A live-run probe, and nothing a shipped host should run.** Nothing on
    /// this machine vibrates, so the only way to exercise the path back to a
    /// peer's controller without a game is to raise an effect ourselves. Holding
    /// A and B on a pad sends one, and zero half a second later.
    pub rumble_probe: bool,
    /// How long a guest keeps the pointer after its last movement, when
    /// [`Config::exclusive_pointer`] is set. Clamped into the bounds in
    /// [`crate::floor`].
    pub exclusive_hold_ms: f64,
    /// Which congestion control level every guest's controller runs at.
    ///
    /// **Level 0 is the most aggressive, not "off".** Its threshold of zero
    /// declares congestion on any stale fragment once the window exceeds the
    /// floor, and it exists for compatibility with an older scheme.
    pub cg_level: usize,
}

/// How many withdrawn attempts are remembered.
///
/// Small and fixed: this exists to catch a withdrawal that overtakes its own
/// offer, which is a race of a few messages, not a history.
const TOMBSTONES: usize = 16;

struct Attempt {
    peer: Peer,
    /// The peer has signalled it is ready. Recorded rather than acted on: the
    /// engine checks whatever candidates arrive whenever they arrive.
    peer_ready: bool,
    /// Buffered until approval, because candidates trickle and the peer starts
    /// sending them before the answer reaches it.
    pending: Vec<SocketAddr>,
    guest: Option<Guest>,
    inject: Option<mpsc::Sender<SocketAddr>>,
    /// Application messages waiting to go to this guest.
    ///
    /// **The body is built where the caller is**, so the thread serving the
    /// guest allocates nothing: what travels here is already the bytes that go
    /// on the wire, terminator included.
    say: Option<mpsc::Sender<Said>>,
    /// The small number this guest is addressed by, kept so a message can be
    /// aimed at it without walking every attempt's thread.
    number: Option<u32>,
}

/// One message on its way to a guest, built where the caller is.
///
/// **Two opcodes share this shape and it is not a coincidence.** An
/// application message and a roster are both a body with its length in the
/// first argument, one number in the second and nothing in the third; only
/// what the number means differs -- a sub-identifier for one, the recipient's
/// own guest number for the other.
#[derive(Debug)]
struct Said {
    opcode: u8,
    /// The second argument. Its meaning belongs to the opcode.
    a1: u32,
    /// Already terminated. The far side reads both as C strings.
    body: Vec<u8>,
}

/// One connected guest, as the seam knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestInfo {
    /// What this guest is addressed by, and what it finds itself in a roster
    /// by.
    pub number: u32,
    pub permissions: lowlat_inject::event::Permissions,
    pub owner: bool,
}

/// A body with the terminator a peer reads it as a C string by, exactly once.
fn terminated(text: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(lowlat_core::control::string_body_len(text.len()));
    body.extend_from_slice(text.strip_suffix(&[0]).unwrap_or(text));
    body.push(0);
    body
}

/// The seam.
#[derive(Debug)]
pub struct Admission {
    config: Config,
    /// Started once, here, because every guest is served from the same encode
    /// (docs/05-host.md section 1) and the seam is what outlives them all.
    stream: Option<Stream>,
    attempts: HashMap<String, Attempt>,
    /// Attempts withdrawn before they were ever registered.
    withdrawn: Vec<String>,
    /// **Handed out once**, to whoever is going to consume it. A poll waits
    /// for as long as its caller asked, and the seam's own lock must not be
    /// held for that long, so the consumer holds the queue directly rather
    /// than reaching it through here (docs/06-api.md 8).
    events: Option<crate::events::Receiver>,
    emit: crate::events::Sender,
    /// Who has the pointer, shared by every guest thread.
    floor: crate::floor::Floor,
    /// **A small number for each guest, and the only identifier that is one.**
    /// An attempt identifier is sixty characters of service-assigned text: it
    /// is what logs correlate on and it is no use naming a device or a pointer
    /// holder, so both use this and one line at admission joins them.
    next_guest: u32,
}

impl core::fmt::Debug for Attempt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attempt")
            .field("pending", &self.pending.len())
            .field("peer_ready", &self.peer_ready)
            .field("running", &self.guest.is_some())
            .finish()
    }
}

impl Admission {
    pub fn new(config: Config) -> Self {
        let (emit, events) = crate::events::queue();
        let stream = config.stream.clone().map(Stream::start);
        let floor =
            crate::floor::Floor::with_hold(config.exclusive_pointer, config.exclusive_hold_ms);
        Self {
            config,
            stream,
            attempts: HashMap::new(),
            withdrawn: Vec::new(),
            events: Some(events),
            emit,
            floor,
            next_guest: 1,
        }
    }

    /// Guests currently admitted, which is what the advertisement publishes.
    pub fn occupancy(&self) -> usize {
        self.attempts.values().filter(|a| a.guest.is_some()).count()
    }

    /// An offer arrived. Register it; the application decides what happens next.
    pub fn new_attempt(&mut self, id: &str, peer: Peer) -> Result<(), Error> {
        // A withdrawal can overtake the offer it withdraws. Admitting one then
        // spends a socket and a thread on a guest that has already gone, and
        // nothing later arrives to say so.
        if let Some(at) = self.withdrawn.iter().position(|seen| seen == id) {
            self.withdrawn.remove(at);
            return Err(Error::Withdrawn);
        }
        if self.occupancy() >= self.config.max_guests {
            return Err(Error::AtCapacity);
        }
        self.attempts.insert(
            id.to_string(),
            Attempt {
                peer,
                peer_ready: false,
                pending: Vec::new(),
                guest: None,
                say: None,
                number: None,
                inject: None,
            },
        );
        Ok(())
    }

    /// A candidate arrived for an attempt.
    ///
    /// **`sync` marks a readiness signal, not an address**, and the flag is
    /// taken rather than left to the caller because the address that rides on
    /// one is arbitrary: a peer is entitled to send a placeholder, and one does
    /// send a literal `1.2.3.4:1234`. Adding that to the table spends checks on
    /// an unrelated host on the public internet.
    ///
    /// Unknown attempts are a no-op rather than an error: an identifier that
    /// has just been torn down is a race with the peer, not a fault.
    pub fn add_candidate(&mut self, id: &str, addr: SocketAddr, sync: bool) {
        let Some(attempt) = self.attempts.get_mut(id) else {
            return;
        };
        if sync {
            attempt.peer_ready = true;
            return;
        }
        match (&attempt.inject, &attempt.guest) {
            (Some(inject), Some(guest)) => {
                if inject.send(addr).is_ok() {
                    // The loop waits on its own deadline, so a candidate that
                    // only lands in the queue is not seen until that expires.
                    let _ = guest.wake_handle().notify();
                }
            }
            // Before approval there is nowhere to put it but the buffer.
            _ => attempt.pending.push(addr),
        }
    }

    /// The application approves. Bind, start connectivity, and hand back the
    /// credentials the answer carries.
    pub fn begin_p2p(&mut self, id: &str) -> Result<HostCredentials, Error> {
        if self.occupancy() >= self.config.max_guests {
            return Err(Error::AtCapacity);
        }
        let attempt = self.attempts.get_mut(id).ok_or(Error::UnknownAttempt)?;
        if attempt.guest.is_some() {
            return Err(Error::AlreadyBegun);
        }

        let local = lowlat_crypto::credentials().map_err(|_| Error::Crypto)?;
        let seed = lowlat_crypto::transaction_seed().map_err(|_| Error::Crypto)?;
        // **Key and nonce prefix together, and both are needed.** The nonce is
        // the credential's four-byte prefix followed by the counter, never
        // four zeros, so a session built from the key alone seals records no
        // peer can open and rejects every record a peer sends. That looks
        // exactly like a path that established and carries nothing.
        let (key, prefix) =
            lowlat_crypto::key_material(&local.aes256).map_err(|_| Error::Crypto)?;
        let mut material = [0u8; MATERIAL_LEN];
        material
            .get_mut(..key.len())
            .ok_or(Error::Crypto)?
            .copy_from_slice(&key);
        material
            .get_mut(key.len()..)
            .ok_or(Error::Crypto)?
            .copy_from_slice(&prefix);

        // Walks from the base, so a second concurrent guest lands on the next
        // free port rather than failing to bind at all.
        let socket = Socket::open(self.config.base_port).map_err(|_| Error::Io)?;
        let port = socket.local_addr().map_err(|_| Error::Io)?.port();
        let wake = Wake::new().map_err(|_| Error::Io)?;

        let (inject, arrivals) = mpsc::channel::<SocketAddr>();
        for addr in attempt.pending.drain(..) {
            let _ = inject.send(addr);
        }
        let (say, said) = mpsc::channel::<Said>();

        let attempt_id = id.to_string();
        let guest_number = self.next_guest;
        self.next_guest = self.next_guest.wrapping_add(1);
        let floor = self.floor.clone();
        let emit = self.emit.clone();
        let servers = self.config.servers.clone();
        let seats = self.stream.as_ref().map(Stream::seats);
        let video = self
            .config
            .stream
            .as_ref()
            .map(|s| (s.width, s.height, s.rotation));
        let ours = (local.ufrag.clone(), local.pwd.clone());
        let theirs = (attempt.peer.ufrag.clone(), attempt.peer.pwd.clone());
        let permissions = attempt.peer.permissions;
        let owner = attempt.peer.owner;
        let rumble_probe = self.config.rumble_probe;

        let guest = Guest::spawn(wake, move |wake, running| {
            run_guest(
                Attached {
                    attempt_id,
                    emit,
                    socket,
                    servers,
                    arrivals,
                    said,
                    ours,
                    theirs,
                    material,
                    seed,
                    seats,
                    video,
                    guest: guest_number,
                    floor,
                    permissions,
                    owner,
                    rumble_probe,
                },
                wake,
                running,
            );
        })
        .map_err(|_| Error::Io)?;

        attempt.inject = Some(inject);
        attempt.guest = Some(guest);
        attempt.say = Some(say);
        attempt.number = Some(guest_number);

        // Emitted with the answer rather than in response to anything, so a
        // peer that withholds its candidates until it sees one is unblocked as
        // early as possible.
        self.emit.send(Event::Ready {
            attempt: id.to_string(),
        });

        Ok(HostCredentials {
            ufrag: local.ufrag,
            pwd: local.pwd,
            fingerprint: local.fingerprint,
            aes256: local.aes256,
            port,
        })
    }

    /// Rejection, withdrawal, disconnect, or reaping an attempt that has just
    /// reported it is over.
    ///
    /// **Emits nothing.** The queue carries what the application did not cause;
    /// this call is the application causing it, and reporting it back produced
    /// a second terminal event for an attempt that had already reported one.
    pub fn end_connection(&mut self, id: &str) {
        match self.attempts.remove(id) {
            Some(mut attempt) => {
                if let Some(guest) = attempt.guest.as_mut() {
                    guest.stop();
                }
            }
            // Withdrawn before it was ever registered. Remembered, because the
            // offer it withdraws may still be in flight behind it.
            None => {
                if !self.withdrawn.iter().any(|seen| seen == id) {
                    if self.withdrawn.len() >= TOMBSTONES {
                        self.withdrawn.remove(0);
                    }
                    self.withdrawn.push(id.to_string());
                }
            }
        }
    }

    /// The next event, or `None`. Never blocks.
    /// Send one guest an application message.
    ///
    /// **The body is built here, on the caller's thread.** What reaches the
    /// thread serving the guest is already the bytes that go on the wire, so
    /// nothing on the path to a peer allocates to send one.
    ///
    /// **The terminator is written whether or not the caller supplied one.**
    /// An established peer reads the body as a C string, so one that ends
    /// without it is read past; and a caller that supplied one must not be
    /// given two, because the length counts them both and the extra byte is
    /// then part of the message.
    ///
    /// Answers whether the guest is one that could be reached at all. A body
    /// past what a peer will accept is refused here rather than being sent and
    /// silently dropped at the far end.
    pub fn send_user_data(&mut self, guest: u32, id: u32, text: &[u8]) -> bool {
        if past_the_cap(text) {
            lowlat_common::log_warn!(
                "guest: refusing an application message of {} bytes for guest {guest}",
                text.len()
            );
            return false;
        }
        let Some(attempt) = self
            .attempts
            .values()
            .find(|attempt| attempt.number == Some(guest))
        else {
            return false;
        };
        let Some(say) = attempt.say.as_ref() else {
            return false;
        };
        say.send(Said {
            opcode: lowlat_core::control::op::USER_DATA,
            a1: id,
            body: terminated(text),
        })
        .is_ok()
    }

    /// Tell every guest who is connected.
    ///
    /// **The same body reaches all of them and the second argument does not**:
    /// each guest is sent its own number alongside, because that is how a peer
    /// finds itself in the list and learns what it is allowed to do. A roster
    /// with nobody's own number in it describes a room the reader is not in.
    ///
    /// The body is built by the caller. It is an encoding this layer has no
    /// business owning, and the crate that would parse it is deliberately
    /// outside the boundary.
    pub fn send_roster(&mut self, body: &[u8]) -> usize {
        let terminated = terminated(body);
        self.attempts
            .values()
            .filter_map(|attempt| Some((attempt.number?, attempt.say.as_ref()?)))
            .filter(|(number, say)| {
                say.send(Said {
                    opcode: lowlat_core::control::op::GUEST_LIST,
                    a1: *number,
                    body: terminated.clone(),
                })
                .is_ok()
            })
            .count()
    }

    /// Who is connected, as much as this layer knows.
    ///
    /// **Only what it decides.** A name, an avatar or an account are the
    /// application's to know; what belongs here is the number a guest is
    /// addressed by, what it is permitted to drive, and whether it owns the
    /// machine.
    pub fn guests(&self) -> Vec<GuestInfo> {
        let mut found: Vec<GuestInfo> = self
            .attempts
            .values()
            .filter_map(|attempt| {
                Some(GuestInfo {
                    number: attempt.number?,
                    permissions: attempt.peer.permissions,
                    owner: attempt.peer.owner,
                })
            })
            .collect();
        // **Ordered, because a map is not.** A roster that reshuffles itself
        // every time it is sent is one a reader cannot diff.
        found.sort_by_key(|guest| guest.number);
        found
    }

    /// What size the stream is really producing, once it is known.
    ///
    /// Nothing until a display has been opened, which is the honest answer
    /// before then: the size is the display's to decide.
    pub fn picture(&self) -> Option<(u32, u32)> {
        self.stream.as_ref().and_then(Stream::picture)
    }

    /// What is being captured, as a checksum of its name.
    ///
    /// **The loop's answer, not the request's.** A guest can ask for one
    /// output and a display can move to another card on its own, and only the
    /// loop knows which of those happened last.
    pub fn captured(&self) -> u32 {
        self.stream.as_ref().map_or(0, Stream::captured)
    }

    /// Capture a different output, or the first one lit.
    ///
    /// **The seam forwards it and judges nothing.** Which outputs exist is the
    /// application's to look up ([`crate::display::Display::outputs`]) and a
    /// name that is not lit is refused where the display is opened, because
    /// that is the only place that can know.
    pub fn select_output(&self, id: Option<String>) {
        if let Some(stream) = self.stream.as_ref() {
            stream.select_output(id);
        }
    }

    /// Send every guest the same application message.
    ///
    /// Answers how many it reached, which is not the guest count: one whose
    /// thread has already gone is not reachable and is not an error.
    pub fn send_user_data_all(&mut self, id: u32, text: &[u8]) -> usize {
        let guests: Vec<u32> = self.attempts.values().filter_map(|a| a.number).collect();
        guests
            .into_iter()
            .filter(|guest| self.send_user_data(*guest, id, text))
            .count()
    }

    pub fn poll_event(&mut self) -> Option<crate::events::Received> {
        self.events.as_ref()?.try_recv()
    }

    /// Change the video settings that do not need anything rebuilt.
    ///
    /// **The output is not one of them and travels separately.** A different
    /// output is a different picture, so it costs the rebuild an output change
    /// has always cost; everything else here reaches the running loop.
    pub fn set_video(&self, video: crate::stream::LiveVideo) {
        if let Some(stream) = self.stream.as_ref() {
            stream.set_video(video);
        }
    }

    /// What the stream is running at now.
    pub fn video(&self) -> Option<crate::stream::LiveVideo> {
        self.stream.as_ref().map(crate::stream::Stream::video)
    }

    /// Take the queue, so a caller can poll it without holding this.
    ///
    /// **Once, and then this stops answering.** Two consumers would each see
    /// part of the stream and each be told a different fraction of what was
    /// dropped, so the queue has one owner and handing it over is what makes
    /// that true rather than a rule somebody has to remember.
    pub fn take_events(&mut self) -> Option<crate::events::Receiver> {
        self.events.take()
    }
}

/// Everything one guest's loop owns for its lifetime.
struct Attached {
    attempt_id: String,
    emit: crate::events::Sender,
    socket: Socket,
    servers: Vec<SocketAddr>,
    arrivals: mpsc::Receiver<SocketAddr>,
    /// Application messages the seam wants sent to this guest.
    said: mpsc::Receiver<Said>,
    ours: (String, String),
    theirs: (String, String),
    /// The 256-bit key and the four-byte nonce prefix that follows it.
    material: [u8; MATERIAL_LEN],
    seed: [u8; 16],
    /// A way onto the stream, taken once this guest is streamable.
    seats: Option<Seats>,
    /// The stream's dimensions and orientation, which the video header
    /// carries.
    video: Option<(u32, u32, lowlat_core::video::Rotation)>,
    /// This guest's small number, used where a name has to be short.
    guest: u32,
    /// Who has the pointer.
    floor: crate::floor::Floor,
    /// What this guest may drive.
    permissions: lowlat_inject::event::Permissions,
    /// Whether it may take the pointer from another guest.
    owner: bool,
    /// The live-run rumble probe.
    rumble_probe: bool,
}

/// Lend one channel's receive storage to the session.
fn attach_recv<'a>(
    session: &mut Session<'a>,
    channel: u8,
    bodies: &'a mut [u8],
    meta: &'a mut [SlotMeta],
) -> bool {
    match RecvRing::new(bodies, meta, SLOT) {
        Ok(ring) => session.attach_recv(channel, ring).is_ok(),
        Err(_) => false,
    }
}

/// Lend one channel's send storage to the session.
fn attach_send<'a>(
    session: &mut Session<'a>,
    channel: u8,
    bodies: &'a mut [u8],
    meta: &'a mut [SendSlot],
) -> bool {
    match SendRing::new(bodies, meta, SLOT, channel) {
        Ok(ring) => session.attach_send(channel, ring).is_ok(),
        Err(_) => false,
    }
}

/// Take everything the peer has said on the control channel.
///
/// **A take that fails is terminal.** The channel only advances when a message
/// is consumed, so a body that does not fit or a prefix that does not parse
/// would be read again on the next pass and every pass after it, at full
/// speed. Ending the attempt reports what a silent spin would not.
/// Notice that this guest no longer has the pointer, and let go of what it
/// was holding with it.
///
/// **This is the whole reason the check is on a timer rather than on a
/// message.** A guest loses the pointer by going quiet, so there is no message
/// of its own to answer, and the release it eventually sends would arrive
/// after somebody else had taken over and be dropped for want of the pointer.
/// A button would stay down on a machine that guest is no longer driving.
fn follow_pointer<S: lowlat_inject::event::Sink>(
    input: &mut Input<S>,
    floor: &crate::floor::Floor,
    guest: u32,
    now_ms: f64,
) {
    // **A button that is still down keeps the pointer.** A guest mid-drag
    // sends nothing while it pauses, so time alone cannot tell a gesture that
    // ended from one that paused; the button can. Without this a pause of half
    // a second hands the cursor to somebody else and takes the first guest's
    // button away in the middle of its own drag.
    if input.injector.holds_pointer_button() {
        floor.refresh(guest, now_ms);
    }
    let holds = floor.holds(guest, now_ms);
    input.injector.set_floor(holds, &mut input.sink);
}

/// What a guest needs to ask for the pointer.
#[derive(Debug, Clone, Copy)]
struct Pointer<'a> {
    floor: &'a crate::floor::Floor,
    guest: u32,
    owner: bool,
    now_ms: f64,
}

/// Opcodes that move the pointer, and so ask for it.
///
/// **Only these.** Keyboards do not conflict the way pointers do and pads are
/// each their own device, so arbitrating either would stop two people using
/// one session for no gain (docs/05-host.md section 7.1).
const fn moves_the_pointer(opcode: u8) -> bool {
    matches!(
        opcode,
        control::op::MOUSE_BUTTON
            | control::op::MOUSE_WHEEL
            | control::op::MOUSE_MOTION
            | control::op::MOUSE_MOTION_STREAM
    )
}

fn drain_control<S: lowlat_inject::event::Sink>(
    session: &mut Session<'_>,
    negotiation: &mut Negotiation,
    mut input: Option<&mut Input<S>>,
    pointer: Pointer<'_>,
    inbound: &mut [u8],
    count: &mut u64,
    said: &mut impl FnMut((u32, Vec<u8>)),
) -> Result<(), Outcome> {
    loop {
        let Some(taken) = session.take_message(CONTROL_CHANNEL, inbound) else {
            return Ok(());
        };
        let Ok(len) = taken else {
            return Err(Outcome::ControlStalled);
        };
        // Counted where it is consumed, so the figure says what the peer sent
        // rather than what parsed.
        *count += 1;
        // A body we cannot parse is skipped rather than fatal: it has already
        // been consumed, so the channel is still moving.
        let Some(content) = inbound.get(..len) else {
            return Err(Outcome::ControlStalled);
        };
        let Ok(message) = control::parse(content) else {
            continue;
        };
        // **A peer that is leaving says so here.** Nothing in signaling
        // reports it, so this message is the only prompt notice there is.
        if message.opcode == control::op::DISCONNECT {
            return Err(Outcome::PeerLeft);
        }
        // **Handed on rather than read.** The sub-identifier and the body are
        // an application's own protocol, and a host that acted on either would
        // be inventing one for it (docs/05-host.md section 5). It is offered
        // to the application and to nothing else here.
        if let Some((id, text)) = control::user_data(&message) {
            said((id, text.to_vec()));
        }
        // **Two consumers, one channel, and no opcode belongs to both.** Each
        // ignores what the other handles, which is why both are offered every
        // message rather than one being given first refusal: ordering them
        // would look like it mattered and nothing would ever exercise it.
        let _ = negotiation.on_control(&message);
        if let Some(input) = input.as_deref_mut() {
            // **Asking is the same as using it.** A guest holds the pointer
            // because it is moving something, and this is the only evidence
            // of that there is.
            if moves_the_pointer(message.opcode) {
                let holds = pointer
                    .floor
                    .claim(pointer.guest, pointer.owner, pointer.now_ms);
                input.injector.set_floor(holds, &mut input.sink);
            }
            input.injector.on_control(&message, &mut input.sink);
        }
    }
}

/// Pass on what the peer declared, and any reinitialization it asked for.
///
/// **Declared first, then the request.** The loop reads every seat's flags
/// when it takes a reinitialization request, so a request that arrived ahead
/// of the flags it is asking for would be answered against the old ones.
///
/// **Taken rather than read**, so one request produces one reinitialization
/// however many times the loop passes before the encoder acts on it.
fn forward_declaration(negotiation: &mut Negotiation, seat: Option<&SeatHold>, declared: &mut u32) {
    let Some(seat) = seat else { return };
    let flags = negotiation.flags();
    if flags != *declared {
        *declared = flags;
        seat.declare(flags);
    }
    if negotiation.take_reconfigure() {
        lowlat_common::log_info!("guest: peer asked to reinitialize, flags={flags:#x}");
        seat.request_reconfigure();
    }
}

/// Ask a peer's controller to vibrate.
fn send_rumble(session: &mut Session<'_>, pad: u32, large: u8, small: u8) {
    send_control(
        session,
        &control::Control {
            a0: pad,
            a1: u32::from(large),
            a2: u32::from(small),
            opcode: control::op::RUMBLE,
            body: &[],
        },
    );
}

/// Whether a body built from this text is past what a peer will take.
///
/// **The terminator counts toward the ceiling**, because it is part of the
/// body that travels: a text of exactly the maximum makes a body one over it.
fn past_the_cap(text: &[u8]) -> bool {
    lowlat_core::control::string_body_len(text.len()) > lowlat_core::control::USER_DATA_MAX
}

/// Send one message that was built elsewhere.
///
/// **The body is written as it was handed over.** Building it belongs where
/// the caller is, so nothing on this thread allocates to send one, and the
/// terminator a peer reads it as a C string by is already there.
fn send_said(session: &mut Session<'_>, message: &Said) {
    send_control(
        session,
        &control::Control {
            // **The length counts the terminator**, which is what a reader
            // treating this as a C string needs, and a body sized by the text
            // alone leaves it reading one byte past what arrived.
            a0: u32::try_from(message.body.len()).unwrap_or(u32::MAX),
            a1: message.a1,
            a2: 0,
            opcode: message.opcode,
            body: &message.body,
        },
    );
}

/// Tell the peer the session is over, and why.
///
/// **Sent before the session is torn down, not after.** The message rides the
/// control channel like any other and needs a turn of the shell to reach the
/// wire; a caller that returns immediately drops it.
fn send_disconnect(session: &mut Session<'_>, reason: i32) {
    #[allow(
        clippy::cast_sign_loss,
        reason = "the status is a signed value carried in an unsigned argument, and the peer                   reads it back as signed"
    )]
    send_control(
        session,
        &control::Control {
            a0: reason as u32,
            a1: 0,
            a2: 0,
            opcode: control::op::DISCONNECT,
            body: &[],
        },
    );
}

/// Notice that the stream's encoder was rebuilt underneath this guest.
///
/// **A new encoder is a new reference chain and a new set of parameter sets.**
/// The peer learns that from the generation the video header carries, so the
/// generation moves here and the announcement follows on the next frame,
/// exactly as it does when a guest first takes a seat. A guest that never
/// noticed would keep naming the encoder that no longer exists.
fn follow_epoch(
    negotiation: &mut Negotiation,
    packetiser: Option<&mut Packetiser>,
    seat: Option<&SeatHold>,
    seen: &mut u32,
) {
    let (Some(seat), Some(packetiser)) = (seat, packetiser) else {
        return;
    };
    let epoch = seat.epoch();
    if epoch == *seen {
        return;
    }
    *seen = epoch;
    packetiser.reconfigured();
    negotiation.encoder_initialised(packetiser.generation());
    lowlat_common::log_info!(
        "guest: encoder reinitialised, generation={}",
        packetiser.generation()
    );
}

/// Queue everything the stream has published to this guest.
///
/// **A refusal is a lost frame and it is reported, never swallowed.** The gate
/// admitted this frame against a window it read one frame ago, so a transport
/// that has moved since can still refuse it; the whole message is refused
/// rather than truncated, so the next predicted frame would reference a
/// picture the peer never received. Only the gate may latch, and the gate is
/// on the stream's thread, so the guest says so and the stream acts on it.
fn send_frames(
    session: &mut Session<'_>,
    seat: &SeatHold,
    packetiser: &mut Packetiser,
    negotiation: &mut Negotiation,
) -> u64 {
    let mut sent = 0u64;
    while let Some(frame) = seat.next_frame() {
        let keyframe = frame.keyframe();
        let Some(header) = packetiser.header(keyframe) else {
            seat.missed_frame();
            continue;
        };
        if session
            .send_message(VIDEO_CHANNEL, header, frame.bytes())
            .is_err()
        {
            seat.missed_frame();
            continue;
        }

        // What the frame owes the peer beyond the picture. Both are cadences
        // rather than per-frame traffic: the latency figure every thirtieth
        // frame, the generation once after an initialisation.
        sent += 1;
        let reports = negotiation.on_frame(seat.encode_latency_ms());
        if let Some(message) = reports.latency_message(0) {
            send_control(session, &message);
        }
        if let Some(message) = reports.generation_message(0) {
            send_control(session, &message);
        }
    }
    sent
}

/// Queue one control message.
///
/// A refusal is dropped rather than reported: everything sent this way is a
/// cadence that repeats, so a message lost to a full control window is
/// replaced by the next one rather than being worth a retry of its own.
fn send_control(session: &mut Session<'_>, message: &control::Control<'_>) {
    let mut header = [0u8; control::CONTROL_HEADER_LEN];
    let Ok(written) = control::encode_header(&mut header, message) else {
        return;
    };
    let Some(head) = header.get(..written) else {
        return;
    };
    let _ = session.send_message(CONTROL_CHANNEL, head, message.body);
}

/// Throughput on the video channel, as the rate controller wants it.
///
/// **Mebibits per second over a measured interval**, and the interval has to
/// be long enough to mean something: sampled every pass, most intervals are a
/// fraction of a millisecond and the figure is noise. Held between recomputes
/// rather than reported as zero, because zero is a claim the path carried
/// nothing.
#[derive(Debug, Default)]
struct Throughput {
    last_bytes: u64,
    last_ms: f64,
    mbps: f64,
}

impl Throughput {
    /// The shortest interval worth dividing by. Half a second is also the
    /// period the controller's increase runs on at sixty frames a second.
    const INTERVAL_MS: f64 = 500.0;

    fn sample(&mut self, bytes: u64, now_ms: f64) -> f64 {
        let elapsed = now_ms - self.last_ms;
        if elapsed < Self::INTERVAL_MS {
            return self.mbps;
        }
        let moved = bytes.saturating_sub(self.last_bytes);
        // Mebibits, not megabits. The controller's peak is compared against
        // this, so the unit has to be the one it was tuned in.
        #[allow(
            clippy::cast_precision_loss,
            reason = "a byte count over half a second; f64 is exact far past it"
        )]
        let bits = (moved * 8) as f64;
        self.mbps = bits / 1_048_576.0 / (elapsed / 1000.0);
        self.last_bytes = bytes;
        self.last_ms = now_ms;
        self.mbps
    }
}

/// No injection, for the paths and the tests that do not have any.
///
/// Named rather than spelled out at each call, because `None` alone leaves the
/// sink's type for the caller to invent.
#[cfg(test)]
const NO_INPUT: Option<&mut Input<Devices>> = None;

/// A guest's virtual devices and the state of what it is holding on them.
///
/// **Kept together because neither half is useful alone**, and because the two
/// have the same lifetime: the devices are created when the guest is admitted
/// and destroyed when its thread returns.
///
/// **Generic over where the events go** so the routing above it can be tested
/// against a recorder rather than against the running session's real devices.
/// The guest thread only ever builds the one variant.
struct Input<S> {
    injector: Injector,
    sink: S,
}

impl Input<Devices> {
    /// Create a guest's devices, or report why not and carry on without them.
    ///
    /// **A session without input is worth more than no session.** Every reason
    /// this fails is a deployment problem on the host rather than anything the
    /// peer did, and refusing the guest would report it as a connection
    /// failure to the one party who cannot fix it.
    fn open(label: &str, video: Option<(u32, u32, Rotation)>) -> Option<Self> {
        let (width, height, rotation) = video?;
        // **Not placed yet, and it does not have to be.** A guest is seated
        // before the loop has opened a display, so the layout is not known
        // here; the loop below picks it up on the pass after it is.
        let extents = desktop_extents(width, height, rotation, None);
        match Devices::create(label) {
            Ok(devices) => Some(Self {
                injector: Injector::new(extents),
                sink: devices,
            }),
            Err(error) => {
                lowlat_common::log_warn!(
                    "inject: no devices for this guest, input is off, error={error}"
                );
                None
            }
        }
    }
}

/// The extents a peer's absolute coordinates arrive in, and where they land.
///
/// **The output's shape, not the encoded frame's**, and on a quarter turn
/// those differ. A peer swaps the dimensions the stream declares before it
/// transforms a pointer position, because what it is looking at is the desktop
/// rather than the buffer, so the coordinates arrive already in that
/// orientation. The host therefore swaps the extents it maps into and rotates
/// nothing. Rotating here as well turns the pointer through a right angle and
/// looks almost right, which is the worst way for it to be wrong.
///
/// **The rectangle needs no turning for the same reason.** It is measured in
/// the desktop the peer is looking at, which is already in that orientation.
fn desktop_extents(width: u32, height: u32, rotation: Rotation, place: Option<Place>) -> Extents {
    let (width, height) = match rotation {
        Rotation::Deg90 | Rotation::Deg270 => (height, width),
        _ => (width, height),
    };
    match place {
        Some(place) => Extents::placed(width, height, place),
        None => Extents::alone(width, height),
    }
}

/// One guest's loop, from approval until teardown.
///
/// Everything it borrows is owned here, on this thread, which is what lets the
/// endpoint hold references into storage allocated once at the top.
fn run_guest(args: Attached, wake: Wake, running: &lowlat_net::Running) {
    // Allocated once, here, and lent to the rings for the life of the thread.
    // **Video receive is absent on purpose**: video is host to guest only, and
    // the group acknowledgement reports zero for an unattached channel, which
    // is the truth about a channel the peer never sends on.
    let mut control_recv_bodies = vec![0u8; SLOT * CONTROL_RECV_SLOTS];
    let mut control_recv_meta = vec![SlotMeta::default(); CONTROL_RECV_SLOTS];
    let mut control_send_bodies = vec![0u8; SLOT * CONTROL_SEND_SLOTS];
    let mut control_send_meta = vec![SendSlot::default(); CONTROL_SEND_SLOTS];
    let mut video_send_bodies = vec![0u8; SLOT * VIDEO_SEND_SLOTS];
    let mut video_send_meta = vec![SendSlot::default(); VIDEO_SEND_SLOTS];
    let mut inbound = vec![0u8; MAX_INBOUND];

    let Ok(envelope) = Envelope::from_credential(&args.material, Cipher::Aes256) else {
        return;
    };
    let mut session = Session::new(envelope, 1, 0.0);
    if !attach_recv(
        &mut session,
        CONTROL_CHANNEL,
        &mut control_recv_bodies,
        &mut control_recv_meta,
    ) || !attach_send(
        &mut session,
        CONTROL_CHANNEL,
        &mut control_send_bodies,
        &mut control_send_meta,
    ) || !attach_send(
        &mut session,
        VIDEO_CHANNEL,
        &mut video_send_bodies,
        &mut video_send_meta,
    ) {
        return;
    }

    let mut conn = Conn::new(
        Credentials {
            local_ufrag: &args.ours.0,
            local_pwd: &args.ours.1,
            remote_ufrag: &args.theirs.0,
            remote_pwd: &args.theirs.1,
        },
        args.seed,
        0.0,
    );
    for server in &args.servers {
        let _ = conn.add_server(*server);
    }

    let mut shell = Shell::new(args.socket, wake, Endpoint::new(conn, session));
    let started = lowlat_common::clock::Time::now();
    let mut reported: Vec<SocketAddr> = Vec::new();
    // The negotiation, from the moment the media path exists. Absent before
    // that, because the five-second deadline runs from there and not from
    // approval: a peer cannot declare itself over a path it does not have.
    let mut negotiation: Option<Negotiation> = None;
    // A place on the stream, taken once this guest has declared itself. Held
    // for the rest of the session and given back by dropping it.
    let mut seat: Option<SeatHold> = None;
    let mut packetiser = args.video.map(|(width, height, rotation)| {
        Packetiser::new(
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(height).unwrap_or(u16::MAX),
            rotation,
        )
    });
    // **Created here, at admission, rather than when the first key arrives.**
    // A device takes a fifth of a second to become usable and there is nothing
    // to wait on; starting now puts that behind connectivity and session
    // initialization, which are longer, so it costs nothing anybody can see.
    let mut input = Input::open(&args.guest.to_string(), args.video);
    if let Some(input) = input.as_mut() {
        // **The one line that joins the two identifiers.** Everything else
        // logs the attempt, and a device name and a pointer holder both need
        // something short; this is where a support question crosses between
        // them.
        lowlat_common::log_info!(
            "guest: number={} attempt={} owner={} keyboard={} pointer={} gamepad={}",
            args.guest,
            args.attempt_id,
            u8::from(args.owner),
            u8::from(args.permissions.keyboard),
            u8::from(args.permissions.pointer),
            u8::from(args.permissions.gamepad)
        );
        input
            .injector
            .set_permissions(args.permissions, &mut input.sink);
    }
    let mut throughput = Throughput::default();
    // What this guest has been told about the pointer, and what it holds.
    let mut pointer = crate::cursor::Sender::new();
    let mut declared = false;
    // What was last published to the seat, so a declaration that has not moved
    // is not republished on every pass.
    let mut declared_flags = 0u32;
    // The picture size this guest is describing, once the stream has settled
    // on one.
    let mut followed: Option<(u16, u16)> = None;
    // Where that picture sits in the desktop, once a session has said.
    let mut placed: Option<Place> = None;
    // The last position published as commanded, so one that has not moved is
    // not republished on every pass.
    let mut commanded: Option<(i32, i32)> = None;
    // Which encoder this guest's peer has been told about. Set from the seat
    // rather than from zero, because a guest that joins after a
    // reinitialization has not missed one.
    let mut seen_epoch = 0u32;
    // Why this session is ending and when that was decided, so the message has
    // time to reach the peer before the session goes away.
    let mut kicked: Option<(i32, f64)> = None;
    let mut inbound_messages: u64 = 0;
    let mut sent: u64 = 0;
    let mut reported_ms = 0.0f64;
    // The live-run probe: when it fires and whether the chord has been let go
    // of since, so holding the two buttons does not rumble continuously.
    let mut probe_until: Option<f64> = None;
    let mut probe_armed = true;
    let mut pass: u32 = 0;

    while !running.stopping() {
        let now = lowlat_common::clock::elapsed_ms(started);
        // Candidates are injected where the application's work is pulled, after
        // the wake has been taken, so nothing enqueued from here on is lost.
        // **The size the stream settled on, checked every pass rather than
        // once.** A guest takes its seat before the loop has opened a display,
        // so at that moment the size is not known yet; and a display can change
        // size while a session is running. The number the peer is told is the
        // coordinate space its absolute input comes back in, so a guest
        // describing the stream with the configured size puts every position
        // through the ratio between the two: a 2560 wide display described as
        // 1920 reaches the right edge three quarters of the way across.
        //
        // **Where the picture sits in the desktop arrives separately and
        // later**, because it is read from the session once the display is
        // open, so the two are followed together rather than one gating the
        // other. Only the size rebuilds the framing; the placement changes
        // nothing a peer can see.
        let settled = seat.as_ref().and_then(SeatHold::picture);
        let situated = seat.as_ref().and_then(SeatHold::place);
        if (settled, situated) != (followed, placed)
            && let Some((width, height)) = settled
            && let Some((_, _, rotation)) = args.video
        {
            if followed != settled {
                lowlat_common::log_info!(
                    "guest: the stream is {width}x{height}, following it for the picture and for \
                     absolute input"
                );
                let mut framing = Packetiser::new(width, height, rotation);
                framing.reconfigured();
                if let Some(negotiation) = negotiation.as_mut() {
                    negotiation.encoder_initialised(framing.generation());
                }
                packetiser = Some(framing);
            }
            if let Some(place) = situated.filter(|_| placed != situated) {
                lowlat_common::log_info!(
                    "guest: the picture is {}x{} at {},{} of a {}x{} desktop, placing input in it",
                    place.width,
                    place.height,
                    place.x,
                    place.y,
                    place.desktop_width,
                    place.desktop_height
                );
            }
            followed = settled;
            placed = situated;
            if let Some(input) = input.as_mut() {
                input.injector.set_extents(desktop_extents(
                    u32::from(width),
                    u32::from(height),
                    rotation,
                    situated,
                ));
            }
        }

        let arrivals = &args.arrivals;
        let seated = seat.as_ref();
        let mut framing = packetiser.as_mut();
        let mut declaring = negotiation.as_mut();
        let outbound = &mut sent;
        // **A loop that stops has to say so.** Leaving here without an
        // outcome strands the attempt: its port, its seat and its share of
        // the advertised capacity are all released by the reaping the event
        // triggers, and nothing else releases them. The session is over either
        // way; the difference is whether the host knows.
        if let Err(error) = shell.turn(now, |endpoint| {
            while let Ok(addr) = arrivals.try_recv() {
                let _ = endpoint.conn().add_candidate(addr);
            }
            if let (Some(seat), Some(packetiser), Some(negotiation)) =
                (seated, framing.as_deref_mut(), declaring.as_deref_mut())
            {
                *outbound += send_frames(endpoint.session(), seat, packetiser, negotiation);
            }
        }) {
            lowlat_common::log_warn!("guest: the transport stopped, err={error}");
            args.emit.send(Event::Ended {
                attempt: args.attempt_id.clone(),
                outcome: Outcome::TransportFailed,
            });
            return;
        }

        // Release anything held while the devices were becoming usable. On
        // its own timer, so a guest that pressed one key and waited does not
        // hold it until it presses a second.
        //
        // **And ask whether this guest still has the pointer**, which is the
        // only way one that stopped moving ever finds out it lost it: it sends
        // nothing further, so there is no message to answer.
        if let Some(input) = input.as_mut() {
            input.sink.tick();
            follow_pointer(input, &args.floor, args.guest, now);
            // **What a local application asks a pad to do goes back to the
            // peer holding it.** Nothing on this machine can vibrate; the
            // controller is somewhere else.
            while let Some(rumble) = input.sink.rumble() {
                send_rumble(
                    shell.endpoint().session(),
                    rumble.pad,
                    rumble.large,
                    rumble.small,
                );
            }
            if args.rumble_probe {
                let chord = input.injector.pad_holding(PROBE_CHORD);
                match (probe_until, chord) {
                    (None, Some(pad)) if probe_armed => {
                        lowlat_common::log_info!("input: rumble probe, pad={pad}");
                        send_rumble(shell.endpoint().session(), pad, u8::MAX, u8::MAX);
                        probe_until = Some(now + PROBE_MS);
                        probe_armed = false;
                    }
                    (Some(until), _) if now >= until => {
                        // **Zero, always.** A controller told to vibrate and
                        // never told to stop vibrates until its battery dies.
                        let pad = chord.unwrap_or(0);
                        send_rumble(shell.endpoint().session(), pad, 0, 0);
                        probe_until = None;
                    }
                    (None, None) => probe_armed = true,
                    _ => {}
                }
            }
        }

        // **Every pass, not on the reporting cadence.** The declaration is on a
        // deadline and input has a human waiting on it, so neither may sit in a
        // ring until a counter comes round.
        if let Some(negotiation) = negotiation.as_mut()
            && let Err(outcome) = drain_control(
                shell.endpoint().session(),
                negotiation,
                input.as_mut(),
                Pointer {
                    floor: &args.floor,
                    guest: args.guest,
                    owner: args.owner,
                    now_ms: now,
                },
                &mut inbound,
                &mut inbound_messages,
                &mut |(id, text)| {
                    args.emit.send(Event::UserData {
                        guest: args.guest,
                        id,
                        text,
                    });
                },
            )
        {
            args.emit.send(Event::Ended {
                attempt: args.attempt_id.clone(),
                outcome,
            });
            return;
        }

        if let Some(negotiation) = negotiation.as_mut() {
            forward_declaration(negotiation, seat.as_ref(), &mut declared_flags);
            follow_epoch(
                negotiation,
                packetiser.as_mut(),
                seat.as_ref(),
                &mut seen_epoch,
            );
        }

        // The path first, then the declaration it carries. A transition seen a
        // cadence late would spend that time off the deadline the peer is
        // being held to.
        match shell.endpoint().conn().state() {
            lowlat_core::conn::State::Established(addr) if negotiation.is_none() => {
                negotiation = Some(Negotiation::opened(now));
                args.emit.send(Event::Established {
                    attempt: args.attempt_id.clone(),
                    addr,
                });
            }
            lowlat_core::conn::State::Failed(_) => {
                args.emit.send(Event::Ended {
                    attempt: args.attempt_id.clone(),
                    outcome: Outcome::ConnectivityFailed,
                });
                return;
            }
            _ => {}
        }

        // **Decided elsewhere, said here.** The stream thread owns no session
        // and cannot write to a peer; this is the only place that can, so the
        // reason it left on the seat becomes a message.
        if kicked.is_none()
            && let Some(reason) = seat.as_ref().and_then(SeatHold::kicked)
        {
            lowlat_common::log_info!("guest: ending this session, reason={reason}");
            send_disconnect(shell.endpoint().session(), reason);
            // The seat goes back now rather than at the end of the grace
            // below: nothing more is going to be sent on it, and a seat held
            // open is capacity another guest cannot have.
            seat = None;
            kicked = Some((reason, now));
        }
        // **The message is on a reliable channel and needs time to get
        // there.** Returning on the pass that queued it tears the session down
        // with the reason still in the ring, and the peer learns nothing.
        if let Some((reason, at)) = kicked
            && now - at >= KICK_GRACE_MS
        {
            args.emit.send(Event::Ended {
                attempt: args.attempt_id.clone(),
                outcome: Outcome::Kicked(reason),
            });
            return;
        }

        if let Some(negotiation) = negotiation.as_mut()
            && negotiation.tick(now) == State::Abandoned
        {
            args.emit.send(Event::Ended {
                attempt: args.attempt_id.clone(),
                outcome: Outcome::NeverDeclared,
            });
            return;
        }

        // What the peer said it can decode, once, where a live run can see
        // it. A stream that renders nothing is diagnosed from here first.
        if !declared && let Some(asked) = negotiation.as_ref().and_then(Negotiation::asked) {
            declared = true;
            // **Read once, here, and never assumed.** A peer that did not say
            // it keeps pointer pictures is sent the picture every time.
            pointer.caches(asked.caches_cursor);
            lowlat_common::log_info!(
                "guest: declared attempt={} max_w={} max_h={} res={}x{} fps={} flags={:#x}",
                args.attempt_id,
                asked.max_width,
                asked.max_height,
                asked.resolution_x,
                asked.resolution_y,
                asked.refresh_rate,
                asked.flags
            );
            // **A peer builds one decoder, from what it declared.** It does
            // not switch on what arrives, so a guest that asked for a codec
            // this stream does not produce will fail to decode every frame it
            // is sent and report a decode error rather than a mismatch. Said
            // plainly here, because from the wire alone it looks like a
            // corrupt stream.
            if asked.hevc() || asked.color444() || asked.ten_bit() {
                lowlat_common::log_warn!(
                    "guest: attempt={} asked for hevc={} 444={} 10bit={}, and this stream is \
                     h264 8-bit 4:2:0; nothing it is sent will decode",
                    args.attempt_id,
                    u8::from(asked.hevc()),
                    u8::from(asked.color444()),
                    u8::from(asked.ten_bit())
                );
            }
        }

        // **A seat is taken when the guest becomes streamable, not when it
        // connects.** A seat held by a peer that has not declared itself is a
        // share of the bitrate budget spent on something that may never
        // decode a frame.
        if seat.is_none()
            && negotiation.as_ref().is_some_and(Negotiation::ready)
            && let Some(seats) = args.seats.as_ref()
            && let Ok(handle) = shell.wake_handle()
        {
            seat = seats.take(handle);
            match (seat.is_some(), negotiation.as_mut(), packetiser.as_ref()) {
                (true, Some(negotiation), Some(packetiser)) => {
                    // **The generation goes out on the frame after this**, so
                    // a peer learns the reference chain started rather than
                    // inferring it from the stream.
                    negotiation.encoder_initialised(packetiser.generation());
                    if let Some(seat) = seat.as_ref() {
                        seen_epoch = seat.epoch();
                    }
                    // Free insurance: thirteen bytes, no body, and a stock
                    // host sends it before its first frame. The peer stores it
                    // and nothing gates on it, so the cost of sending it is
                    // the cost of being wrong about that.
                    send_control(
                        shell.endpoint().session(),
                        &control::Control {
                            a0: 1,
                            a1: 0,
                            a2: 0,
                            opcode: control::op::HOST_MODE,
                            body: &[],
                        },
                    );
                }
                // **Capacity is the one refusal that used to be silent.** The
                // offer was accepted and the path was built, and then the
                // guest sat connected receiving nothing until its own liveness
                // deadline noticed, minutes later.
                (false, _, _) => {
                    lowlat_common::log_warn!("guest: every seat is taken");
                    send_disconnect(shell.endpoint().session(), status::NO_ROOM);
                    kicked = Some((status::NO_ROOM, now));
                }
                _ => lowlat_common::log_warn!("guest: seated with nothing to send on"),
            }
        }

        // **Where this guest told the pointer to be**, which is what the
        // hotspot is derived from: nothing reports a pointer's hotspot, and
        // the difference between a command and where the display then drew the
        // pointer is exactly it. Read from the injector rather than from the
        // message, so a position the permission gate or the arbiter refused is
        // not reported as one the pointer was put at.
        if let Some(seat) = seat.as_ref()
            && let Some((x, y)) = input.as_ref().and_then(|i| i.injector.commanded())
            && commanded != Some((x, y))
        {
            commanded = Some((x, y));
            seat.command_pointer(
                u16::try_from(x).unwrap_or(u16::MAX),
                u16::try_from(y).unwrap_or(u16::MAX),
            );
        }

        // **The pointer, per guest, because what it is owed depends on what it
        // already holds.** The stream reads it once; this decides whether the
        // picture travels or only its name, and sends nothing at all on the
        // passes where nothing about it moved.
        // **Whether this guest is the one driving**, which decides what it is
        // shown: a guest without the arbitrated pointer sees a refused shape
        // rather than finding out by nothing happening.
        let holds = args.floor.holds(args.guest, now);
        if let Some(seat) = seat.as_ref()
            && let Some(message) = pointer.next(seat, holds)
        {
            let _ = shell
                .endpoint()
                .session()
                .send_message(CONTROL_CHANNEL, message, &[]);
        }

        // **Whatever the application asked to be sent, before anything else
        // this pass.** It is rare and small; draining it here keeps it off the
        // frame path and out of the shell's own closure.
        while let Ok(message) = args.said.try_recv() {
            send_said(shell.endpoint().session(), &message);
        }

        // What the stream's controller and gate are steered by. Cheap, and it
        // reads state this loop already owns.
        if let Some(seat) = seat.as_ref()
            && let Some((window, stale, bytes)) =
                shell.endpoint().session().send_pressure(VIDEO_CHANNEL)
        {
            let measured = throughput.sample(bytes, now);
            seat.report(window, stale, measured);

            // **The line a live run is read from.** Frames leaving, the window
            // the gate is judging, and what the path is actually carrying: a
            // stream that stops is one of the three going flat.
            if now - reported_ms >= PROGRESS_MS {
                reported_ms = now;
                // **What the peer is still doing, not only what we are.** A
                // guest that stops acknowledging while its own messages keep
                // arriving is alive and has stopped reading; one that goes
                // silent both ways has torn the session down. The two have
                // different causes and the log has to tell them apart.
                let rx = shell
                    .endpoint()
                    .session()
                    .recv_cumulative(CONTROL_CHANNEL)
                    .unwrap_or(0);
                let input_tally = input
                    .as_ref()
                    .map(|i| i.injector.tally())
                    .unwrap_or_default();
                lowlat_common::log_info!(
                    "guest: attempt={} frames={sent} window={window} stale={stale} mbps={measured:.2} encode_ms={:.2} rx_frag={rx} rx_msg={inbound_messages} keys={} btn={} wheel={} motion={} pad={}",
                    args.attempt_id,
                    seat.encode_latency_ms(),
                    input_tally.keys,
                    input_tally.buttons,
                    input_tally.wheels,
                    input_tally.motions,
                    input_tally.pads
                );
            }
        }

        pass = pass.wrapping_add(1);
        if pass % REPORT_EVERY != 0 {
            continue;
        }

        // Trickled as they are found. The engine retains them, so this reports
        // what is new rather than what was returned by one call.
        let fresh: Vec<SocketAddr> = shell
            .endpoint()
            .conn()
            .reflexive()
            .filter(|addr| !reported.contains(addr))
            .collect();
        for addr in fresh {
            reported.push(addr);
            args.emit.send(Event::Candidate {
                attempt: args.attempt_id.clone(),
                addr,
                from_stun: true,
            });
        }

        // Only once there is a path. Before that nothing has arrived by
        // definition, so liveness would read as dead from the first pass.
        if negotiation.is_some() {
            let outcome = match shell.endpoint().health(now) {
                lowlat_core::session::Health::Dead => Some(Outcome::PeerGone),
                // **The window says this and nothing else does.** A peer that
                // keeps acknowledging while receiving nothing is alive by
                // every inbound measure, and everything queued for it is
                // retransmitted until something ends the session.
                lowlat_core::session::Health::Undeliverable => {
                    // **Both channels, because either can be the one that
                    // stopped.** A peer whose control channel keeps up while
                    // its video ring backs up is a different fault from a path
                    // that has gone away, and these two numbers are what tell
                    // them apart afterwards.
                    let video = shell
                        .endpoint()
                        .session()
                        .send_pressure(VIDEO_CHANNEL)
                        .unwrap_or_default();
                    let control = shell
                        .endpoint()
                        .session()
                        .send_pressure(CONTROL_CHANNEL)
                        .unwrap_or_default();
                    lowlat_common::log_warn!(
                        "guest: nothing acknowledged for {DELIVERY_DEADLINE_MS:.0} ms, \
                         video window={} stale={}, control window={} stale={}",
                        video.0,
                        video.1,
                        control.0,
                        control.1
                    );
                    Some(Outcome::Undeliverable)
                }
                _ => None,
            };
            if let Some(outcome) = outcome {
                args.emit.send(Event::Ended {
                    attempt: args.attempt_id.clone(),
                    outcome,
                });
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests_support {
    use super::Peer;

    pub(super) fn peer() -> Peer {
        Peer {
            permissions: lowlat_inject::event::Permissions::default(),
            owner: false,
            ufrag: "aaaa".into(),
            pwd: "passwordforaaaa".into(),
            aes256: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::*;
    use super::*;

    fn admission(max_guests: usize) -> Admission {
        Admission::new(Config {
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: crate::floor::HOLD_MS,
            cg_level: 1,
            // Ephemeral, so the tests do not fight the machine for a fixed port.
            base_port: 0,
            max_guests,
            servers: Vec::new(),
            stream: None,
        })
    }

    #[test]
    fn an_unknown_attempt_is_a_no_op_rather_than_a_fault() {
        let mut seam = Admission::new(Config {
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: crate::floor::HOLD_MS,
            cg_level: 1,
            base_port: 0,
            max_guests: 1,
            servers: Vec::new(),
            stream: None,
        });
        // A candidate for an identifier that never existed, or has just been
        // torn down, is a race with the peer and not an error.
        seam.add_candidate("nope", "127.0.0.1:9000".parse().unwrap(), false);
        assert_eq!(seam.begin_p2p("nope").unwrap_err(), Error::UnknownAttempt);
    }

    #[test]
    fn capacity_is_the_only_policy_and_it_is_applied_at_admission() {
        let mut seam = admission(1);
        seam.new_attempt("one", peer()).expect("register");
        seam.new_attempt("two", peer()).expect("register");
        seam.begin_p2p("one").expect("first guest");
        // Registering was fine; admitting past the limit is not.
        assert_eq!(seam.begin_p2p("two").unwrap_err(), Error::AtCapacity);
        assert_eq!(seam.occupancy(), 1);
        seam.end_connection("one");
    }

    /// Candidates trickle, and the peer starts sending them before the answer
    /// has reached it. One that arrives before approval must be kept, not
    /// dropped: on a wide-area path it may be the only one that works.
    #[test]
    fn candidates_arriving_before_approval_are_kept() {
        let mut seam = admission(4);
        seam.new_attempt("a", peer()).expect("register");
        seam.add_candidate("a", "203.0.113.9:41000".parse().unwrap(), false);
        seam.add_candidate("a", "203.0.113.9:41001".parse().unwrap(), false);

        let buffered = seam.attempts.get("a").map(|a| a.pending.len());
        assert_eq!(buffered, Some(2), "a candidate before approval was dropped");

        seam.begin_p2p("a").expect("approve");
        let drained = seam.attempts.get("a").map(|a| a.pending.len());
        assert_eq!(drained, Some(0), "buffered candidates were not handed over");
        seam.end_connection("a");
    }

    /// Generated at approval, bound to the socket just opened for the attempt.
    /// Earlier binds them to nothing; per registration leaks state for attempts
    /// that are never approved.
    #[test]
    fn credentials_appear_at_approval_and_are_not_reused() {
        let mut seam = admission(4);
        seam.new_attempt("a", peer()).expect("register");
        seam.new_attempt("b", peer()).expect("register");
        let first = seam.begin_p2p("a").expect("approve a");
        let second = seam.begin_p2p("b").expect("approve b");

        assert_eq!(first.ufrag.len(), 8);
        assert_eq!(first.pwd.len(), 32);
        assert_eq!(first.fingerprint.len(), 64);
        assert_eq!(first.aes256.len(), 254);
        assert_ne!(first.aes256, second.aes256, "two guests shared a media key");
        assert_ne!(first.ufrag, second.ufrag);

        seam.end_connection("a");
        seam.end_connection("b");
    }

    /// One socket per guest, so a second concurrent guest cannot have the base
    /// port and walks to the next. A host that cannot walk cannot admit a
    /// second guest at all on a configured port. *Named regression test.*
    #[test]
    fn concurrent_guests_land_on_consecutive_ports() {
        // A base the first guest can actually take. Ephemeral would give each
        // guest an unrelated port and prove nothing about walking.
        let probe = Socket::open(0).expect("probe");
        let base = probe.local_addr().expect("addr").port();
        drop(probe);

        let mut seam = Admission::new(Config {
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: crate::floor::HOLD_MS,
            cg_level: 1,
            base_port: base,
            max_guests: 4,
            servers: Vec::new(),
            stream: None,
        });
        seam.new_attempt("a", peer()).expect("register a");
        seam.new_attempt("b", peer()).expect("register b");
        seam.new_attempt("c", peer()).expect("register c");

        let a = seam.begin_p2p("a").expect("approve a").port;
        let b = seam.begin_p2p("b").expect("approve b").port;
        let c = seam.begin_p2p("c").expect("approve c").port;

        assert_eq!(a, base, "the first guest did not take the configured port");
        assert_eq!(b, base + 1, "the second guest did not walk");
        assert_eq!(c, base + 2, "the third guest did not walk");

        seam.end_connection("a");
        seam.end_connection("b");
        seam.end_connection("c");
    }

    /// A readiness marker is not a candidate. A peer is entitled to put a
    /// placeholder address on one, and one sends a literal `1.2.3.4:1234`;
    /// checking that address spends the attempt on an unrelated host.
    /// *Named regression test.*
    #[test]
    fn a_sync_marker_never_becomes_a_candidate() {
        let mut seam = admission(4);
        seam.new_attempt("a", peer()).expect("register");
        seam.add_candidate("a", "1.2.3.4:1234".parse().unwrap(), true);

        let attempt = seam.attempts.get("a").expect("attempt");
        assert!(attempt.peer_ready, "the marker was not recorded");
        assert_eq!(
            attempt.pending.len(),
            0,
            "a readiness marker was queued as a candidate"
        );
    }

    /// A peer may withhold every real candidate until it has seen one of ours,
    /// so approval must produce it without being prompted.
    /// *Named regression test.*
    #[test]
    fn approval_asks_for_a_readiness_marker_to_be_sent() {
        let mut seam = admission(4);
        seam.new_attempt("a", peer()).expect("register");
        seam.begin_p2p("a").expect("approve");

        let mut saw_ready = false;
        while let Some(received) = seam.poll_event() {
            if received.event
                == (Event::Ready {
                    attempt: "a".to_string(),
                })
            {
                saw_ready = true;
            }
        }
        assert!(saw_ready, "approval did not ask for a readiness marker");
        seam.end_connection("a");
    }

    /// A withdrawal can overtake the offer it withdraws. Admitting the offer
    /// then spends a socket and a thread on a guest that has already gone, and
    /// nothing later arrives to say so -- the port stays held for the life of
    /// the host. *Named regression test.*
    #[test]
    fn a_withdrawal_that_overtakes_its_offer_refuses_the_offer() {
        let mut seam = admission(4);
        seam.end_connection("late");
        assert_eq!(
            seam.new_attempt("late", peer()).unwrap_err(),
            Error::Withdrawn
        );
        assert_eq!(seam.occupancy(), 0);

        // Only the once: a second offer with that identifier is a new attempt.
        seam.new_attempt("late", peer())
            .expect("the tombstone is consumed");
        seam.end_connection("late");
    }

    /// **The queue is handed over once, and the seam stops answering for it.**
    /// A poll waits for as long as its caller asked and the seam's lock must
    /// not be held for that long, so whoever polls holds the queue itself. Two
    /// consumers would each see part of the stream and each be told a
    /// different fraction of what was lost.
    #[test]
    fn the_event_queue_is_handed_over_once_and_carries_what_the_seam_raises() {
        let mut seam = admission(4);
        let events = seam.take_events().expect("the queue is there to take");
        assert!(
            seam.take_events().is_none(),
            "the queue was handed to a second consumer"
        );

        seam.new_attempt("a", peer()).expect("register");
        seam.begin_p2p("a").expect("approve");

        // It reaches the holder rather than the seam, which now reports
        // nothing at all.
        assert!(seam.poll_event().is_none(), "the seam still answers polls");
        let taken = events
            .recv_timeout(core::time::Duration::from_secs(5))
            .expect("approval raises the readiness marker");
        assert_eq!(
            taken.event,
            Event::Ready {
                attempt: "a".to_string()
            }
        );
        seam.end_connection("a");
    }

    #[test]
    fn ending_an_attempt_frees_the_slot_without_reporting_it() {
        let mut seam = admission(1);
        seam.new_attempt("a", peer()).expect("register");
        seam.begin_p2p("a").expect("approve");
        seam.end_connection("a");

        assert_eq!(seam.occupancy(), 0);
        // The application caused this, so it needs no event back. A second
        // terminal event for an attempt that already reported one is what this
        // asserts against.
        let mut terminal = 0;
        while let Some(received) = seam.poll_event() {
            if matches!(received.event, Event::Ended { .. }) {
                terminal += 1;
            }
        }
        assert_eq!(terminal, 0, "an application-initiated end reported itself");
    }
}

/// The ring geometry and the control path, driven through real rings rather
/// than against the constants that configure them.
#[cfg(test)]
mod geometry {
    use super::*;
    use lowlat_core::control::{Control, op};

    const KEY: [u8; 32] = [7u8; 32];

    /// Storage for one endpoint, in the shape [`run_guest`] builds.
    struct Arena {
        control_recv_bodies: Vec<u8>,
        control_recv_meta: Vec<SlotMeta>,
        control_send_bodies: Vec<u8>,
        control_send_meta: Vec<SendSlot>,
        video_send_bodies: Vec<u8>,
        video_send_meta: Vec<SendSlot>,
    }

    impl Arena {
        fn new() -> Self {
            Self {
                control_recv_bodies: vec![0u8; SLOT * CONTROL_RECV_SLOTS],
                control_recv_meta: vec![SlotMeta::default(); CONTROL_RECV_SLOTS],
                control_send_bodies: vec![0u8; SLOT * CONTROL_SEND_SLOTS],
                control_send_meta: vec![SendSlot::default(); CONTROL_SEND_SLOTS],
                video_send_bodies: vec![0u8; SLOT * VIDEO_SEND_SLOTS],
                video_send_meta: vec![SendSlot::default(); VIDEO_SEND_SLOTS],
            }
        }

        /// A peer's session: the same rings, plus a receive ring on video so
        /// what a guest sends can be read back the way a client reads it.
        fn peer<'a>(&'a mut self, video_recv: &'a mut VideoRecv) -> Session<'a> {
            let envelope = Envelope::from_key(&KEY).expect("envelope");
            let mut session = Session::new(envelope, 1, 0.0);
            assert!(attach_recv(
                &mut session,
                CONTROL_CHANNEL,
                &mut self.control_recv_bodies,
                &mut self.control_recv_meta,
            ));
            assert!(attach_send(
                &mut session,
                CONTROL_CHANNEL,
                &mut self.control_send_bodies,
                &mut self.control_send_meta,
            ));
            assert!(attach_recv(
                &mut session,
                VIDEO_CHANNEL,
                &mut video_recv.bodies,
                &mut video_recv.meta,
            ));
            session
        }

        /// A session wired exactly as a guest's is.
        fn session(&mut self) -> Session<'_> {
            let envelope = Envelope::from_key(&KEY).expect("envelope");
            let mut session = Session::new(envelope, 1, 0.0);
            assert!(attach_recv(
                &mut session,
                CONTROL_CHANNEL,
                &mut self.control_recv_bodies,
                &mut self.control_recv_meta,
            ));
            assert!(attach_send(
                &mut session,
                CONTROL_CHANNEL,
                &mut self.control_send_bodies,
                &mut self.control_send_meta,
            ));
            assert!(attach_send(
                &mut session,
                VIDEO_CHANNEL,
                &mut self.video_send_bodies,
                &mut self.video_send_meta,
            ));
            session
        }
    }

    /// The video receive ring a peer has and a host does not.
    struct VideoRecv {
        bodies: Vec<u8>,
        meta: Vec<SlotMeta>,
    }

    impl VideoRecv {
        fn new() -> Self {
            Self {
                bodies: vec![0u8; SLOT * VIDEO_SEND_SLOTS],
                meta: vec![SlotMeta::default(); VIDEO_SEND_SLOTS],
            }
        }
    }

    /// A body the initialization parser accepts, in the shape a peer sends.
    fn declaration() -> Vec<u8> {
        let mut body = br#"{"_version":1,"_max_w":1920,"_max_h":1080,"_flags":8,"resolutionX":1920,"resolutionY":1080,"refreshRate":60}"#.to_vec();
        // The terminating NUL a peer puts on it, which the parser strips.
        body.push(0);
        body
    }

    fn control_bytes(control: &Control<'_>) -> Vec<u8> {
        let mut header = [0u8; lowlat_core::control::CONTROL_HEADER_LEN];
        let written = lowlat_core::control::encode_header(&mut header, control).expect("header");
        let mut out = header[..written].to_vec();
        out.extend_from_slice(control.body);
        out
    }

    /// Move every datagram one session has to send into the other, and report
    /// the largest one that crossed.
    fn pump(from: &mut Session<'_>, into: &mut Session<'_>, now_ms: f64) -> usize {
        let mut datagram = [0u8; lowlat_core::MAX_DATAGRAM];
        let mut scratch = [0u8; lowlat_core::MAX_DATAGRAM];
        let mut largest = 0usize;
        while let Some(result) = from.get_output(now_ms, &mut datagram) {
            let len = result.expect("output");
            largest = largest.max(len);
            let wire = datagram.get(..len).expect("written");
            let _ = into.process_input(wire, now_ms, &mut scratch);
        }
        largest
    }

    /// **The slot width is the fragment width, so it is also the datagram
    /// width.** A ring sized for headroom does not gain headroom; it emits
    /// datagrams no probe has justified, and a peer that cannot take one
    /// discards the whole datagram rather than truncating it. Widening `SLOT`
    /// past the floor fails this. *Named regression test.*
    #[test]
    fn no_emitted_datagram_exceeds_the_floor_the_peer_is_known_to_accept() {
        let mut arena = Arena::new();
        let mut ours = arena.session();
        let mut theirs = Arena::new();
        let mut theirs = theirs.session();

        // Big enough to fragment many times over, so the full-size fragment is
        // the common case here rather than the exception.
        let unit = vec![0xA5u8; 200 * SLOT];
        ours.send_message(VIDEO_CHANNEL, &[], &unit).expect("queue");

        let largest = pump(&mut ours, &mut theirs, 1.0);
        assert!(
            largest <= lowlat_core::DEFAULT_DATAGRAM,
            "emitted a {largest}-byte datagram, past the {}-byte floor",
            lowlat_core::DEFAULT_DATAGRAM
        );
        // And the fragments really were full, or the bound above is vacuous.
        assert_eq!(largest, lowlat_core::DEFAULT_DATAGRAM);
    }

    /// The video ring is the peer's ring depth, which is also the delivery
    /// gate's top ceiling: a frame the gate admits must fit the ring it is
    /// admitted into. Sizing the ring from control traffic fails this.
    /// *Named regression test.*
    #[test]
    fn the_video_ring_holds_a_frame_as_large_as_the_gate_will_admit() {
        let mut arena = Arena::new();
        let mut session = arena.session();

        let ceiling = crate::gate::ceiling(30.0) as usize;
        assert_eq!(
            ceiling, VIDEO_SEND_SLOTS,
            "the ring and the gate's top ceiling have parted company"
        );

        // Exactly the ceiling's worth of fragments. **The length prefix rides
        // in the first fragment**, so the largest frame that fits is four
        // bytes short of the arithmetic anyone would write down.
        let frame = vec![0u8; ceiling * SLOT - lowlat_core::message::LENGTH_PREFIX_LEN];
        let queued = session
            .send_message(VIDEO_CHANNEL, &[], &frame)
            .expect("a frame at the ceiling was refused");
        assert_eq!(queued as usize, ceiling);

        // And nothing beyond it: the window is the peer's depth, not ours to
        // exceed.
        assert!(
            session.send_message(VIDEO_CHANNEL, &[], &[0u8; 1]).is_err(),
            "the ring accepted a fragment past the peer's ring depth"
        );
    }

    /// Records what a guest's devices would have been written.
    #[derive(Debug, Default)]
    struct Recorder {
        events: Vec<(lowlat_inject::event::Device, lowlat_inject::event::Event)>,
    }

    impl lowlat_inject::event::Sink for Recorder {
        fn emit(
            &mut self,
            device: lowlat_inject::event::Device,
            events: &[lowlat_inject::event::Event],
        ) {
            self.events
                .extend(events.iter().map(|event| (device, *event)));
        }
    }

    /// A pointer nobody is arbitrating, for the tests that are not about it.
    fn no_pointer() -> Pointer<'static> {
        static FREE: std::sync::OnceLock<crate::floor::Floor> = std::sync::OnceLock::new();
        Pointer {
            floor: FREE.get_or_init(|| crate::floor::Floor::new(false)),
            guest: 1,
            owner: false,
            now_ms: 0.0,
        }
    }

    fn recording_input() -> Input<Recorder> {
        Input {
            injector: Injector::new(Extents::alone(1920, 1080)),
            sink: Recorder::default(),
        }
    }

    /// Send control messages from a peer and drain them into the host.
    fn drain_from_peer(
        input: Option<&mut Input<Recorder>>,
        messages: &[Control<'_>],
    ) -> Negotiation {
        let mut ours = Arena::new();
        let mut ours = ours.session();
        let mut theirs = Arena::new();
        let mut theirs = theirs.session();
        for message in messages {
            theirs
                .send_message(CONTROL_CHANNEL, &[], &control_bytes(message))
                .expect("queue");
        }
        pump(&mut theirs, &mut ours, 1.0);

        let mut negotiation = Negotiation::opened(0.0);
        let mut inbound = vec![0u8; MAX_INBOUND];
        drain_control(
            &mut ours,
            &mut negotiation,
            input,
            no_pointer(),
            &mut inbound,
            &mut 0,
            &mut |_| {},
        )
        .expect("drained");
        negotiation
    }

    /// **Both consumers of the control channel get what is theirs.** The
    /// declaration and the input share one channel and one drain, and before
    /// there was anywhere to put input the drain counted it as unhandled and
    /// dropped it. Everything below this is covered by the inject crate; what
    /// is covered nowhere else is that the guest loop hands the message over
    /// at all, and that adding a second consumer did not cost the first one.
    #[test]
    fn both_consumers_of_the_control_channel_get_what_is_theirs() {
        let mut input = recording_input();
        let body = declaration();
        let negotiation = drain_from_peer(
            Some(&mut input),
            &[
                Control {
                    a0: 0,
                    a1: 0,
                    a2: 0,
                    opcode: op::INIT,
                    body: &body,
                },
                Control {
                    a0: 4,
                    a1: 0,
                    a2: 1,
                    opcode: op::KEYBOARD,
                    body: &[],
                },
            ],
        );

        assert!(negotiation.ready(), "the declaration was not taken");
        let keys: Vec<(u16, i32)> = input
            .sink
            .events
            .iter()
            .filter(|(_, e)| e.kind == 0x01)
            .map(|(_, e)| (e.code, e.value))
            .collect();
        assert_eq!(keys, vec![(30, 1)], "the keystroke did not reach injection");
    }

    /// Two guests through the real drain, sharing one arbiter.
    fn two_guests_move(exclusive: bool, gap_ms: f64) -> (Recorder, Recorder) {
        let floor = crate::floor::Floor::new(exclusive);
        let motion = Control {
            a0: 0,
            a1: 100,
            a2: 100,
            opcode: op::MOUSE_MOTION,
            body: &[],
        };
        let mut first = recording_input();
        let mut second = recording_input();
        // The first moves, the second tries while the first still holds it,
        // and then the first moves again.
        for (guest, at) in [(1u32, 0.0), (2, gap_ms), (1, gap_ms + 1.0)] {
            let input = if guest == 1 { &mut first } else { &mut second };
            drain_one(
                input,
                Pointer {
                    floor: &floor,
                    guest,
                    owner: false,
                    now_ms: at,
                },
                &motion,
            );
        }
        (first.sink, second.sink)
    }

    fn drain_one(input: &mut Input<Recorder>, pointer: Pointer<'_>, message: &Control<'_>) {
        let mut ours = Arena::new();
        let mut ours = ours.session();
        let mut theirs = Arena::new();
        let mut theirs = theirs.session();
        theirs
            .send_message(CONTROL_CHANNEL, &[], &control_bytes(message))
            .expect("queue");
        pump(&mut theirs, &mut ours, 1.0);
        let mut negotiation = Negotiation::opened(0.0);
        let mut inbound = vec![0u8; MAX_INBOUND];
        drain_control(
            &mut ours,
            &mut negotiation,
            Some(input),
            pointer,
            &mut inbound,
            &mut 0,
            &mut |_| {},
        )
        .expect("drained");
    }

    /// **With it off, both guests drive at once**, which is the default and
    /// the ordinary case of two people sharing a desktop.
    #[test]
    fn both_guests_drive_the_pointer_when_it_is_not_arbitrated() {
        let (first, second) = two_guests_move(false, 1.0);
        assert!(!first.events.is_empty());
        assert!(!second.events.is_empty());
    }

    /// With it on, the one that moved first keeps it while it keeps moving.
    #[test]
    fn the_guest_that_moved_first_keeps_the_pointer() {
        let (first, second) = two_guests_move(true, 1.0);
        assert!(
            !first.events.is_empty(),
            "the first guest lost its own pointer"
        );
        assert!(
            second.events.is_empty(),
            "a second guest drove at the same time"
        );
    }

    /// And hands it over once it stops.
    #[test]
    fn the_pointer_goes_to_the_next_guest_once_the_first_stops() {
        let (_first, second) = two_guests_move(true, crate::floor::HOLD_MS);
        assert!(!second.events.is_empty(), "the pointer never changed hands");
    }

    /// **The guest that lost the pointer lets go of what it was holding.** It
    /// went quiet, so nothing it sends will ever say so; only the pass notices.
    /// Without this a button stays down on a machine somebody else is driving,
    /// because the release arrives after the handover and is dropped with the
    /// rest of that guest's pointer input.
    #[test]
    fn a_guest_that_loses_the_pointer_lets_go_of_its_buttons() {
        let floor = crate::floor::Floor::new(true);
        let mut holder = recording_input();
        let press = Control {
            a0: 1,
            a1: 1,
            a2: 0,
            opcode: op::MOUSE_BUTTON,
            body: &[],
        };
        drain_one(
            &mut holder,
            Pointer {
                floor: &floor,
                guest: 1,
                owner: false,
                now_ms: 0.0,
            },
            &press,
        );
        assert!(!holder.sink.events.is_empty(), "the press never landed");
        holder.sink.events.clear();

        // It goes quiet, and somebody else takes over.
        let later = crate::floor::HOLD_MS + 1.0;
        assert!(floor.claim(2, false, later));

        follow_pointer(&mut holder, &floor, 1, later);
        let released: Vec<u16> = holder
            .sink
            .events
            .iter()
            .filter(|(_, e)| e.kind == 0x01 && e.value == 0)
            .map(|(_, e)| e.code)
            .collect();
        assert_eq!(released, vec![0x110], "the button was left down");
    }

    /// The whole defect, through the drain: a guest holds a button, pauses
    /// longer than the hold, and must still have the pointer and its button.
    #[test]
    fn a_paused_drag_keeps_both_the_pointer_and_its_button() {
        let floor = crate::floor::Floor::new(true);
        let mut dragger = recording_input();
        drain_one(
            &mut dragger,
            Pointer {
                floor: &floor,
                guest: 1,
                owner: false,
                now_ms: 0.0,
            },
            &Control {
                a0: 1,
                a1: 1,
                a2: 0,
                opcode: op::MOUSE_BUTTON,
                body: &[],
            },
        );
        dragger.sink.events.clear();

        // It pauses, for four times the hold, saying nothing at all.
        let mut now = 0.0;
        for _ in 0..8 {
            now += crate::floor::HOLD_MS / 2.0;
            follow_pointer(&mut dragger, &floor, 1, now);
        }
        assert!(
            dragger.sink.events.is_empty(),
            "a paused drag had its button taken away"
        );
        assert!(
            !floor.claim(2, false, now),
            "another guest took the pointer mid-drag"
        );
    }

    /// A guest that still has it is left alone. **It has to be holding
    /// something for this to mean anything**: a pass that released
    /// unconditionally would emit nothing here either, and the check would
    /// pass while every button came up on every pass.
    #[test]
    fn a_guest_that_still_has_the_pointer_is_not_disturbed() {
        let floor = crate::floor::Floor::new(true);
        let mut holder = recording_input();
        drain_one(
            &mut holder,
            Pointer {
                floor: &floor,
                guest: 1,
                owner: false,
                now_ms: 0.0,
            },
            &Control {
                a0: 1,
                a1: 1,
                a2: 0,
                opcode: op::MOUSE_BUTTON,
                body: &[],
            },
        );
        assert!(!holder.sink.events.is_empty(), "the press never landed");
        holder.sink.events.clear();

        follow_pointer(&mut holder, &floor, 1, 10.0);
        assert!(
            holder.sink.events.is_empty(),
            "a guest still holding the pointer had its button taken away"
        );
    }

    /// **A quarter turn swaps the extents and nothing else.** The peer sends
    /// coordinates in the desktop's orientation already; a host that also
    /// rotated them would put the pointer at a right angle to where it was
    /// asked for, which reads as an input bug rather than a geometry one.
    #[test]
    fn a_quarter_turn_swaps_the_extents_and_a_half_turn_does_not() {
        let upright = desktop_extents(1920, 1080, Rotation::None, None);
        assert_eq!((upright.width, upright.height), (1920, 1080));
        for rotation in [Rotation::Deg90, Rotation::Deg270] {
            let turned = desktop_extents(1920, 1080, rotation, None);
            assert_eq!((turned.width, turned.height), (1080, 1920), "{rotation:?}");
        }
        // A half turn leaves the shape alone, so swapping there would be a
        // pointer that works upright and breaks upside down.
        let half = desktop_extents(1920, 1080, Rotation::Deg180, None);
        assert_eq!((half.width, half.height), (1920, 1080));
    }

    /// **The terminator is written whether or not the caller supplied one**,
    /// and never twice. An established peer reads the body as a C string, so a
    /// body that ends without one is read past; and the declared length counts
    /// both, so a second terminator becomes part of the message.
    #[test]
    fn an_application_message_is_terminated_exactly_once() {
        for text in [&b"hello"[..], b"hello\0"] {
            let mut body = Vec::new();
            body.extend_from_slice(text.strip_suffix(&[0]).unwrap_or(text));
            body.push(0);
            assert_eq!(body, b"hello\0", "text={text:?}");
            assert_eq!(
                body.len(),
                lowlat_core::control::string_body_len(5),
                "the length a peer is told must count the terminator once"
            );
        }
    }

    /// **A message longer than a peer will take is refused before it is sent.**
    /// The far end drops it without a word, so a sender that does not check
    /// loses the message with no way to find out.
    ///
    /// **Tested at the boundary and not through the send**, which was tried
    /// and proved nothing: with no guest to aim at, a message refused for its
    /// length and one refused for having nowhere to go are the same answer,
    /// and removing the check entirely left the test passing.
    #[test]
    fn the_cap_counts_the_terminator_that_travels_with_the_text() {
        let max = lowlat_core::control::USER_DATA_MAX;
        assert!(
            !past_the_cap(&vec![b'x'; max - 1]),
            "the largest body that fits was refused"
        );
        assert!(
            past_the_cap(&vec![b'x'; max]),
            "a text of exactly the maximum makes a body one byte over it"
        );
    }

    /// A guest that is not there is not an error, and not a send either.
    #[test]
    fn an_application_message_to_nobody_reaches_nobody() {
        let mut seam = Admission::new(Config {
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: crate::floor::HOLD_MS,
            cg_level: 1,
            base_port: 0,
            max_guests: 1,
            servers: Vec::new(),
            stream: None,
        });
        assert!(!seam.send_user_data(1, 0, b"hello"));
        assert_eq!(seam.send_user_data_all(0, b"hello"), 0);
    }

    /// A guest with no devices is still a guest: its input is dropped and
    /// everything else about the session carries on.
    #[test]
    fn a_guest_without_devices_still_negotiates() {
        let body = declaration();
        let negotiation = drain_from_peer(
            None,
            &[Control {
                a0: 0,
                a1: 0,
                a2: 0,
                opcode: op::INIT,
                body: &body,
            }],
        );
        assert!(negotiation.ready());
    }

    /// **The declaration arrives on channel 0, and nothing else reports it.**
    /// A guest whose control channel is unattached has its declaration counted
    /// as unhandled and dropped, while the group acknowledgement reports zero
    /// for that channel forever, so the peer retransmits it until it gives up.
    /// Attaching video alone fails this. *Named regression test.*
    #[test]
    fn a_peers_declaration_reaches_the_negotiation_through_the_real_rings() {
        let mut ours = Arena::new();
        let mut ours = ours.session();
        let mut theirs = Arena::new();
        let mut theirs = theirs.session();

        let body = declaration();
        let message = control_bytes(&Control {
            a0: 0,
            a1: 0,
            a2: 0,
            opcode: op::INIT,
            body: &body,
        });
        theirs
            .send_message(CONTROL_CHANNEL, &[], &message)
            .expect("queue");
        pump(&mut theirs, &mut ours, 1.0);

        let mut negotiation = Negotiation::opened(0.0);
        let mut inbound = vec![0u8; MAX_INBOUND];
        drain_control(
            &mut ours,
            &mut negotiation,
            NO_INPUT,
            no_pointer(),
            &mut inbound,
            &mut 0,
            &mut |_| {},
        )
        .expect("drained");

        assert!(
            negotiation.ready(),
            "the declaration did not reach the negotiation"
        );
        let asked = negotiation.asked().expect("what the peer asked for");
        assert_eq!((asked.max_width, asked.max_height), (1920, 1080));
        assert_eq!(asked.refresh_rate, 60);
    }

    /// **The guest's whole outbound half, end to end.** A coded frame is
    /// framed, fragmented, sealed, carried, reassembled, and read back as the
    /// video message a client reads: the header at the offsets section 11.3
    /// fixes, then the bitstream, byte for byte.
    ///
    /// Every earlier check stops short of this. The packetiser's own tests
    /// read the header out of a fragment it wrote; the corpus comparison
    /// checks headers against a recording. Neither sends anything through a
    /// send ring, and the send ring is where the fragment sizing and the
    /// window live.
    #[test]
    fn a_coded_frame_crosses_to_a_peer_whole() {
        let mut ours = Arena::new();
        let mut ours = ours.session();
        let mut theirs = Arena::new();
        let mut video = VideoRecv::new();
        let mut theirs = theirs.peer(&mut video);

        // Big enough to fragment many times, with content that is a function
        // of its offset so a misplaced fragment cannot pass.
        let unit: Vec<u8> = (0..40_000u32).map(|at| (at % 251) as u8).collect();
        let mut packetiser = Packetiser::new(1920, 1080, lowlat_core::video::Rotation::None);
        let header = packetiser.header(true).expect("header").to_vec();
        ours.send_message(VIDEO_CHANNEL, &header, &unit)
            .expect("queue");

        // Several passes: the outstanding cap releases a bounded number of
        // fragments per pass, so one drain does not carry a frame this size.
        let mut taken = Vec::new();
        for round in 0..64 {
            let now = f64::from(round) * 20.0;
            pump(&mut ours, &mut theirs, now);
            pump(&mut theirs, &mut ours, now);
            let mut out = vec![0u8; 64 * 1024];
            if let Some(Ok(len)) = theirs.take_message(VIDEO_CHANNEL, &mut out) {
                out.truncate(len);
                taken = out;
                break;
            }
        }

        assert!(!taken.is_empty(), "the frame never arrived whole");
        let (head, body) = taken.split_at(lowlat_core::video::VIDEO_HEADER_LEN);
        let parsed = lowlat_core::video::parse(head).expect("a video header");
        assert_eq!((parsed.width, parsed.height), (1920, 1080));
        assert_eq!(parsed.rotation, lowlat_core::video::Rotation::None);
        assert!(
            !parsed.ten_bit,
            "an eight-bit stream claimed ten-bit colour on the wire"
        );
        assert_eq!(body, unit, "the bitstream did not arrive intact");
    }

    /// **The generation a peer is told is the generation its frames carry.**
    /// Announced on the frame after the encoder is ready and repeated in every
    /// video header, so the two are read from one place or they disagree, and
    /// a peer that trusted either would be tracking a reference chain that
    /// does not exist. *Named regression test.*
    #[test]
    fn the_announced_generation_is_the_one_the_header_carries() {
        let mut packetiser = Packetiser::new(1920, 1080, lowlat_core::video::Rotation::None);
        let mut negotiation = Negotiation::opened(0.0);
        let body = declaration();
        let raw = control_bytes(&Control {
            a0: 0,
            a1: 0,
            a2: 0,
            opcode: op::INIT,
            body: &body,
        });
        let declared = control::parse(&raw).expect("parsed");
        assert!(negotiation.on_control(&declared));
        assert!(negotiation.ready());

        negotiation.encoder_initialised(packetiser.generation());
        let reports = negotiation.on_frame(3.0);
        let announced = reports.generation_message(0).expect("an announcement");
        assert_eq!(announced.opcode, op::ENCODER_GENERATION);
        assert_eq!(announced.a0, 0, "the stream index is argument zero");

        let header = packetiser.header(true).expect("header");
        let carried = lowlat_core::video::parse(header).expect("a video header");
        assert_eq!(
            announced.a1, carried.frame_id,
            "the announcement and the header disagree about the generation"
        );

        // Once only. A second announcement would tell a peer the chain
        // restarted when nothing did.
        assert!(negotiation.on_frame(3.0).generation_message(0).is_none());
    }

    /// A take that does not fit does not consume the message, so the same one
    /// is read again on the next pass and every pass after it. Reporting it
    /// costs a session; spinning on it costs a core, silently.
    /// *Named regression test.*
    #[test]
    fn a_message_too_long_to_take_ends_the_attempt_rather_than_spinning() {
        let mut ours = Arena::new();
        let mut ours = ours.session();
        let mut theirs = Arena::new();
        let mut theirs = theirs.session();

        let body = vec![b'x'; 4096];
        theirs
            .send_message(CONTROL_CHANNEL, &[], &body)
            .expect("queue");
        pump(&mut theirs, &mut ours, 1.0);

        let mut negotiation = Negotiation::opened(0.0);
        // Shorter than the message that arrived, which is the condition the
        // real buffer is sized to avoid.
        let mut inbound = vec![0u8; 1024];
        assert_eq!(
            drain_control(
                &mut ours,
                &mut negotiation,
                NO_INPUT,
                no_pointer(),
                &mut inbound,
                &mut 0,
                &mut |_| {}
            ),
            Err(Outcome::ControlStalled),
            "a message that cannot be taken was not reported"
        );
    }
}

#[cfg(test)]
mod reclamation {
    use super::tests_support::*;
    use super::*;

    /// A guest that ends gives its port back, so the next one lands on the base
    /// again rather than walking past it. *Named regression test.*
    ///
    /// The failure this guards is slow and silent: a host that never reclaims
    /// walks one port further per session and, after enough of them, cannot
    /// admit anyone at all. It took a live run to notice, because every short
    /// test admits one guest and stops.
    #[test]
    fn a_port_is_reclaimed_when_its_guest_ends() {
        let probe = Socket::open(0).expect("probe");
        let base = probe.local_addr().expect("addr").port();
        drop(probe);

        let mut seam = Admission::new(Config {
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: crate::floor::HOLD_MS,
            cg_level: 1,
            base_port: base,
            max_guests: 4,
            servers: Vec::new(),
            stream: None,
        });

        let mut ports = Vec::new();
        for round in 0..4 {
            let id = format!("guest-{round}");
            seam.new_attempt(&id, peer()).expect("register");
            ports.push(seam.begin_p2p(&id).expect("approve").port);
            seam.end_connection(&id);
        }

        assert_eq!(
            ports,
            vec![base; 4],
            "the port walked instead of being reclaimed"
        );
        assert_eq!(seam.occupancy(), 0);
    }
}
