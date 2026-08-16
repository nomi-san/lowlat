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
use lowlat_core::endpoint::Endpoint;
use lowlat_core::envelope::Envelope;
use lowlat_core::send::{SendRing, SendSlot};
use lowlat_core::session::Session;
use lowlat_net::{Guest, Shell, Socket, Wake};

/// Ring geometry for one guest.
///
/// Provisional: sized for control traffic, which is all that crosses before
/// there is an encoder. The video channel needs far more and will size itself
/// when there is something to put in it.
const SLOT: usize = 1400;
const SLOTS: usize = 256;
const CHANNEL: u8 = 1;

/// How often the loop reports what it has gathered, in passes. Cheap, and only
/// reads state the loop already owns.
const REPORT_EVERY: u32 = 16;

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
}

/// What the offer told us about the peer.
#[derive(Debug, Clone)]
pub struct Peer {
    pub ufrag: String,
    pub pwd: String,
    /// Media key material. Retained because the answer's key is ours, not
    /// theirs; this is kept for the legacy path and for logging its absence.
    pub aes256: Option<String>,
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
}

/// The seam.
#[derive(Debug)]
pub struct Admission {
    config: Config,
    attempts: HashMap<String, Attempt>,
    /// Attempts withdrawn before they were ever registered.
    withdrawn: Vec<String>,
    events: mpsc::Receiver<Event>,
    emit: mpsc::Sender<Event>,
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
        let (emit, events) = mpsc::channel();
        Self {
            config,
            attempts: HashMap::new(),
            withdrawn: Vec::new(),
            events,
            emit,
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
        let (key, _prefix) =
            lowlat_crypto::key_material(&local.aes256).map_err(|_| Error::Crypto)?;

        // Walks from the base, so a second concurrent guest lands on the next
        // free port rather than failing to bind at all.
        let socket = Socket::open(self.config.base_port).map_err(|_| Error::Io)?;
        let port = socket.local_addr().map_err(|_| Error::Io)?.port();
        let wake = Wake::new().map_err(|_| Error::Io)?;

        let (inject, arrivals) = mpsc::channel::<SocketAddr>();
        for addr in attempt.pending.drain(..) {
            let _ = inject.send(addr);
        }

        let attempt_id = id.to_string();
        let emit = self.emit.clone();
        let servers = self.config.servers.clone();
        let ours = (local.ufrag.clone(), local.pwd.clone());
        let theirs = (attempt.peer.ufrag.clone(), attempt.peer.pwd.clone());

        let guest = Guest::spawn(wake, move |wake, running| {
            run_guest(
                Attached {
                    attempt_id,
                    emit,
                    socket,
                    servers,
                    arrivals,
                    ours,
                    theirs,
                    key,
                    seed,
                },
                wake,
                running,
            );
        })
        .map_err(|_| Error::Io)?;

        attempt.inject = Some(inject);
        attempt.guest = Some(guest);

        // Emitted with the answer rather than in response to anything, so a
        // peer that withholds its candidates until it sees one is unblocked as
        // early as possible.
        let _ = self.emit.send(Event::Ready {
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
    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

/// Everything one guest's loop owns for its lifetime.
struct Attached {
    attempt_id: String,
    emit: mpsc::Sender<Event>,
    socket: Socket,
    servers: Vec<SocketAddr>,
    arrivals: mpsc::Receiver<SocketAddr>,
    ours: (String, String),
    theirs: (String, String),
    key: [u8; 32],
    seed: [u8; 16],
}

/// One guest's loop, from approval until teardown.
///
/// Everything it borrows is owned here, on this thread, which is what lets the
/// endpoint hold references into storage allocated once at the top.
fn run_guest(args: Attached, wake: Wake, running: &lowlat_net::Running) {
    let mut recv_bodies = vec![0u8; SLOT * SLOTS];
    let mut recv_meta = vec![SlotMeta::default(); SLOTS];
    let mut send_bodies = vec![0u8; SLOT * SLOTS];
    let mut send_meta = vec![SendSlot::default(); SLOTS];

    let Ok(envelope) = Envelope::from_key(&args.key) else {
        return;
    };
    let mut session = Session::new(envelope, 1, 0.0);
    if session
        .attach_recv(
            CHANNEL,
            match RecvRing::new(&mut recv_bodies, &mut recv_meta, SLOT) {
                Ok(ring) => ring,
                Err(_) => return,
            },
        )
        .is_err()
    {
        return;
    }
    if session
        .attach_send(
            CHANNEL,
            match SendRing::new(&mut send_bodies, &mut send_meta, SLOT, CHANNEL) {
                Ok(ring) => ring,
                Err(_) => return,
            },
        )
        .is_err()
    {
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
    let mut settled = false;
    let mut pass: u32 = 0;

    while !running.stopping() {
        let now = lowlat_common::clock::elapsed_ms(started);
        // Candidates are injected where the application's work is pulled, after
        // the wake has been taken, so nothing enqueued from here on is lost.
        let arrivals = &args.arrivals;
        let Ok(_) = shell.turn(now, |endpoint| {
            while let Ok(addr) = arrivals.try_recv() {
                let _ = endpoint.conn().add_candidate(addr);
            }
        }) else {
            break;
        };

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
            let _ = args.emit.send(Event::Candidate {
                attempt: args.attempt_id.clone(),
                addr,
                from_stun: true,
            });
        }

        // Only once there is a path. Before that nothing has arrived by
        // definition, so liveness would read as dead from the first pass.
        if settled && shell.endpoint().health(now) == lowlat_core::session::Health::Dead {
            let _ = args.emit.send(Event::Ended {
                attempt: args.attempt_id.clone(),
                outcome: Outcome::PeerGone,
            });
            return;
        }

        match shell.endpoint().conn().state() {
            lowlat_core::conn::State::Established(addr) if !settled => {
                settled = true;
                let _ = args.emit.send(Event::Established {
                    attempt: args.attempt_id.clone(),
                    addr,
                });
            }
            lowlat_core::conn::State::Failed(_) => {
                let _ = args.emit.send(Event::Ended {
                    attempt: args.attempt_id.clone(),
                    outcome: Outcome::ConnectivityFailed,
                });
                return;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests_support {
    use super::Peer;

    pub(super) fn peer() -> Peer {
        Peer {
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
            // Ephemeral, so the tests do not fight the machine for a fixed port.
            base_port: 0,
            max_guests,
            servers: Vec::new(),
        })
    }

    #[test]
    fn an_unknown_attempt_is_a_no_op_rather_than_a_fault() {
        let mut seam = admission(4);
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
            base_port: base,
            max_guests: 4,
            servers: Vec::new(),
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
        while let Some(event) = seam.poll_event() {
            if event
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
        while let Some(event) = seam.poll_event() {
            if matches!(event, Event::Ended { .. }) {
                terminal += 1;
            }
        }
        assert_eq!(terminal, 0, "an application-initiated end reported itself");
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
            base_port: base,
            max_guests: 4,
            servers: Vec::new(),
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
