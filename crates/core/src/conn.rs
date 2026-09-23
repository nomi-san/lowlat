//! The punch: candidates in, checks out, a path or a typed failure.
//!
//! Sans-IO like the rest of the core. It owns no socket, so an output carries
//! the destination and the send-time TTL alongside the bytes, and the shell
//! applies both. That is not bookkeeping: a probe is deliberately emitted with
//! a TTL too low to reach the peer, and a shell that leaves the socket at that
//! value caps the media path at a few hops. The obligation to restore is
//! visible in the type rather than remembered.
//!
//! This is not full ICE and implementing full ICE would be wrong. There are no
//! candidate pairs with computed priorities, no check list with frozen and
//! waiting states, no nomination, and no role conflict resolution, because the
//! role is a fixed value. See docs/03-connectivity.md 1.
//!
//! The schedule is tight and deliberately so:
//!
//! ```text
//! t=0      the attempt begins
//! once     one probe toward the reflexive candidate, at a TTL too low
//!          to leave the local network
//! every    500 ms per candidate, an authenticated check
//! t=7500   no answer, the attempt is over
//! ```
//!
//! Fifteen checks per candidate is the entire budget. There is no slow retry
//! tier behind it, which is why a candidate that cannot possibly answer must
//! never be admitted in the first place.

use core::net::{IpAddr, SocketAddr};

use sha1::{Digest, Sha1};

use crate::error::{Error, Result};
use crate::stun::{self, Message, Method, TransactionId};

/// How often a single candidate is rechecked.
pub const CHECK_CADENCE_MS: f64 = 500.0;
/// How long the whole attempt may run before it is declared a failure.
pub const PUNCH_WINDOW_MS: f64 = 7_500.0;
/// Shortest gap between two emitted datagrams, so a burst of candidates does
/// not leave as a burst of packets.
pub const PACING_MS: f64 = 10.0;
/// TTL for the mapping probe. High enough to cross the local network, far too
/// low to reach the peer.
pub const PROBE_TTL: u8 = 4;

/// Candidates held for one attempt. Past this an arrival is dropped, which is
/// correct: with a 7500 ms window there is no budget to check more.
pub const MAX_CANDIDATES: usize = 16;

/// Responses owed to a peer that are not yet on the wire.
///
/// Sixteen, because a peer running a full agent checks every pair it holds
/// in one burst and keeps checking after a path exists, and an answer that
/// is dropped for want of a slot reads to that peer as a pair that failed.
const MAX_PENDING: usize = 16;

/// Reflexive servers consulted for our own mapped address.
pub const MAX_SERVERS: usize = 4;

/// How a datagram must be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ttl {
    /// Leave the socket at its configured value.
    Default,
    /// Lower the socket to [`PROBE_TTL`] for this datagram, then **restore it**.
    Probe,
}

/// One datagram, ready to send, with what the socket must do to send it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Egress {
    /// Where it goes.
    pub to: SocketAddr,
    /// How it must be sent.
    pub ttl: Ttl,
    /// How much of the caller's buffer was written.
    pub len: usize,
    /// The source address to send from, or `None` to let the kernel choose.
    ///
    /// A check answer names the address the check arrived at, and a session
    /// keeps sending from the address its path was proven at. On a host with
    /// several addresses the kernel's own choice follows the routing table,
    /// which is free to pick a sibling address -- the peer's filter then sees
    /// a source it never probed and drops what the check just earned.
    pub from: Option<IpAddr>,
}

/// Why an attempt ended without a path.
///
/// Typed, never a bare timeout, because the correct response differs
/// completely between them. See docs/03-connectivity.md 9.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Failure {
    /// Checks were sent and nothing answered. The only outcome that justifies
    /// escalating to a relay.
    ProbeTimeout,
    /// The attempt had no candidate to check before the window closed.
    NoCandidates,
}

/// Where the attempt has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum State {
    /// Checking whatever candidates have arrived.
    Checking,
    /// A candidate answered and is now the path. Checks stop.
    Established(SocketAddr),
    /// Over.
    Failed(Failure),
}

/// What an inbound datagram turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound {
    /// A peer checked us. A response is queued.
    CheckAnswered,
    /// A peer answered our check and this address became the path.
    PathEstablished(SocketAddr),
    /// A reflexive server reported the address it sees us at. Emit it to the
    /// application as a candidate.
    Reflexive(SocketAddr),
    /// Authenticated, but it told us nothing new.
    Redundant,
}

/// What kind of candidate an address is, as far as the punch cares.
///
/// The exchange marks every candidate with two flags, and all three of their
/// meaningful combinations arrive from real peers, so all three are modeled:
/// direct (the lan flag, which a peer also sets on every IPv6 address --
/// there is no translation to negotiate on that family however the address
/// was found), server-reflexive (the from-stun flag), and neither -- a
/// translated-path guess no server verified, typically the peer's public
/// address at its local port, offered in case its translator preserves
/// ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Kind {
    /// Routable as given: host candidates, every IPv6 address, and the
    /// observed source of a verified check. Never the probe's target.
    Direct,
    /// The peer's server-reflexive address: the path that crosses
    /// translation, and the mapping probe's only target.
    Reflexive,
    /// A translated-path address no server verified. It crosses translation,
    /// so it is not direct; it is unverified, so the one probe is not spent
    /// on it.
    Wan,
}

impl Kind {
    /// Classify from the exchange's two flags.
    ///
    /// The lan flag wins when both are set, mirroring how the direct
    /// behaviors short-circuit ahead of the translated ones on the far side.
    /// Both set has never been observed on a wire.
    pub fn marked(lan: bool, reflexive: bool) -> Self {
        if lan {
            Kind::Direct
        } else if reflexive {
            Kind::Reflexive
        } else {
            Kind::Wan
        }
    }
}

/// Credentials for one attempt, from the signaling exchange.
///
/// Two passwords, and mixing them up produces a connection that authenticates
/// nothing while appearing to work: a request we send is signed with the peer's
/// password, and a request we receive was signed with ours.
#[derive(Debug, Clone, Copy)]
pub struct Credentials<'a> {
    /// Our fragment, which the peer names first when it checks us.
    pub local_ufrag: &'a str,
    /// Our password. Signs the responses we emit and verifies inbound checks.
    pub local_pwd: &'a str,
    /// The peer's fragment.
    pub remote_ufrag: &'a str,
    /// The peer's password. Signs the checks we emit and verifies their
    /// responses.
    pub remote_pwd: &'a str,
}

/// Checks the window's budget allows one target: fifteen at the cadence,
/// and the mapping probe can add one more.
const SENT_IDS: usize = 16;

/// Every identifier one target has been asked with, kept for the life of
/// the attempt.
///
/// An answer is matched against all of them, because a true answer can be
/// slower than the cadence that emits the next identifier: matched against
/// only the newest, a path whose round trip exceeds the cadence fails
/// deterministically, the answer forever one identifier behind. The budget
/// bounds the set, so the storage is fixed; were it ever exceeded, the
/// oldest is overwritten, which narrows matching by one rather than
/// anything worse.
#[derive(Debug, Clone, Copy)]
struct SentIds {
    slots: [Option<TransactionId>; SENT_IDS],
    at: usize,
}

impl SentIds {
    const fn new() -> Self {
        Self {
            slots: [None; SENT_IDS],
            at: 0,
        }
    }

    fn push(&mut self, tid: TransactionId) {
        if let Some(slot) = self.slots.get_mut(self.at) {
            *slot = Some(tid);
        }
        self.at = (self.at + 1) % SENT_IDS;
    }

    fn contains(&self, tid: TransactionId) -> bool {
        self.slots.iter().flatten().any(|sent| *sent == tid)
    }
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    addr: SocketAddr,
    kind: Kind,
    /// When a check was last emitted, or `None` if none ever was.
    last_check_ms: Option<f64>,
    /// Every check emitted toward this candidate, matched against answers.
    sent: SentIds,
}

#[derive(Debug, Clone, Copy)]
struct Server {
    addr: SocketAddr,
    last_probe_ms: Option<f64>,
    /// Every probe emitted toward this server. An answer carries no
    /// credentials, so this set and the source address are the whole of what
    /// admits one.
    sent: SentIds,
    answered: bool,
    /// The address this server reported seeing us at, once it has.
    mapped: Option<SocketAddr>,
}

/// One punch attempt.
#[derive(Debug)]
pub struct Conn<'a> {
    credentials: Credentials<'a>,
    /// Mixed into every transaction identifier. Supplied by the shell, which is
    /// where entropy lives; the core stays free of a random number generator
    /// so a run is reproducible from this value alone.
    seed: [u8; 16],

    candidates: [Option<Candidate>; MAX_CANDIDATES],
    servers: [Option<Server>; MAX_SERVERS],
    /// Answers owed: the peer's address, the transaction to echo, and the
    /// local address the request arrived at, which is the address the answer
    /// must leave from.
    pending: [Option<(SocketAddr, TransactionId, Option<IpAddr>)>; MAX_PENDING],

    state: State,
    /// Whether the peer has said it is listening. Full-length checks toward
    /// translated-path candidates wait for this; direct candidates and the
    /// probe do not.
    peer_ready: bool,
    /// The local address the winning answer arrived at, latched when the path
    /// is chosen and reused for every send after it. `None` until then, and
    /// `None` for the whole attempt if the shell could not say.
    local: Option<IpAddr>,
    started_ms: f64,
    /// When the last datagram left, for pacing.
    last_sent_ms: Option<f64>,
    /// The mapping probe is emitted once per attempt, not once per candidate.
    probe_sent: bool,
    counter: u32,
}

impl<'a> Conn<'a> {
    /// Begin an attempt. The window starts now.
    pub fn new(credentials: Credentials<'a>, seed: [u8; 16], now_ms: f64) -> Self {
        Self {
            credentials,
            seed,
            candidates: [None; MAX_CANDIDATES],
            servers: [None; MAX_SERVERS],
            pending: [None; MAX_PENDING],
            state: State::Checking,
            peer_ready: false,
            local: None,
            started_ms: now_ms,
            last_sent_ms: None,
            probe_sent: false,
            counter: 0,
        }
    }

    /// Where the attempt has got to.
    pub fn state(&self) -> State {
        self.state
    }

    /// Every address a reflexive server has reported seeing us at.
    ///
    /// These are our server-reflexive candidates, and the application forwards
    /// each one to the peer as it appears. They are retained rather than only
    /// reported once through [`Inbound::Reflexive`], because a caller that
    /// processes datagrams in batches has nowhere to put a one-shot result and
    /// would drop the candidate that matters most on a wide-area path.
    ///
    /// Two servers reporting two different addresses is also how endpoint
    /// independent mapping is told from symmetric, so the set is the answer
    /// rather than any single entry.
    pub fn reflexive(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.servers.iter().flatten().filter_map(|s| s.mapped)
    }

    /// The chosen path, once there is one.
    pub fn path(&self) -> Option<SocketAddr> {
        match self.state {
            State::Established(addr) => Some(addr),
            _ => None,
        }
    }

    /// The local address the path was proven at, once there is one.
    ///
    /// Everything sent for the rest of the session leaves from this address;
    /// the kernel's own selection follows the routing table and is free to
    /// move to a sibling address, which the peer's filter never probed.
    pub fn local(&self) -> Option<IpAddr> {
        self.local
    }

    /// The peer's readiness marker arrived: it is bound and listening.
    ///
    /// Until then, full-length checks go only to direct candidates. A check
    /// that reaches a translated path before the peer has sent anything
    /// outward is unsolicited traffic to its translator, which can commit a
    /// state entry that then blocks the peer's own punch -- so the checks
    /// that cross translation wait, while a direct candidate (reachable as
    /// given, no translator to poison) and the mapping probe (which never
    /// reaches the peer at all) do not.
    pub fn set_peer_ready(&mut self) {
        self.peer_ready = true;
    }

    /// Whether a candidate of `kind` may be sent full-length checks yet.
    fn checkable(&self, kind: Kind) -> bool {
        kind == Kind::Direct || self.peer_ready
    }

    /// Offer a remote candidate.
    ///
    /// Candidates trickle in as the peer discovers them, so this is called
    /// repeatedly and at any time. A duplicate is ignored and a full table
    /// drops the arrival rather than evicting something already being checked.
    pub fn add_candidate(&mut self, addr: SocketAddr, kind: Kind) -> Result<()> {
        let addr = stun::canonical(addr);
        if self.candidates.iter().flatten().any(|c| c.addr == addr) {
            return Ok(());
        }
        let slot = self
            .candidates
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Oversized)?;
        *slot = Some(Candidate {
            addr,
            kind,
            last_check_ms: None,
            sent: SentIds::new(),
        });
        Ok(())
    }

    /// How many candidates are being checked.
    pub fn candidate_count(&self) -> usize {
        self.candidates.iter().flatten().count()
    }

    /// Add a reflexive server to ask for our own mapped address.
    ///
    /// Optional. An attempt with none still punches; it simply has nothing but
    /// whatever candidates the application gathered locally to offer.
    pub fn add_server(&mut self, addr: SocketAddr) -> Result<()> {
        let addr = stun::canonical(addr);
        if self.servers.iter().flatten().any(|s| s.addr == addr) {
            return Ok(());
        }
        let slot = self
            .servers
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Oversized)?;
        *slot = Some(Server {
            addr,
            last_probe_ms: None,
            sent: SentIds::new(),
            answered: false,
            mapped: None,
        });
        Ok(())
    }

    /// Feed one datagram that classified as a connectivity check.
    ///
    /// `from` is the address it actually arrived from, which is the only
    /// trustworthy source: a peer behind address translation cannot know it.
    /// `local` is the address it arrived *at*, when the shell can say -- the
    /// address an answer must leave from, and the one the session keeps once
    /// the path is chosen.
    pub fn process_input(
        &mut self,
        datagram: &[u8],
        from: SocketAddr,
        local: Option<IpAddr>,
    ) -> Result<Inbound> {
        let from = stun::canonical(from);
        // A v4 arrival on a dual-stack socket may be reported v4-mapped;
        // collapse it the same way the peer address is.
        let local = local.map(|ip| match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            IpAddr::V4(_) => ip,
        });
        let message = Message::parse(datagram)?;

        match message.method() {
            Method::BindingRequest => {
                // Signed with our password, because from the peer's side we are
                // the remote. A request that fails is dropped without a
                // response rather than answered with an error.
                if !message.verify(self.credentials.local_pwd) {
                    return Err(Error::Decrypt);
                }

                // The source of a verified check is reachable by definition:
                // it is where the datagram actually came from. Under symmetric
                // translation it is the *only* address that is, because what the
                // peer advertised was created toward a reflexive server and its
                // packets to us leave from a different mapping entirely. Without
                // this, such a peer connects from its side while we never find a
                // path, and a host that never finds a path never sends media.
                //
                // Admission rests on the check having authenticated, which means
                // whoever sent it holds the password from the credential
                // exchange. Nothing weaker would do: an unauthenticated source
                // address is an invitation to point us anywhere.
                //
                // A full table refuses the addition, which is correct and not
                // fatal: the check is still answered, so the peer can still
                // reach us on a path it already had.
                let _ = self.add_candidate(from, Kind::Direct);

                self.queue_response(from, message.transaction_id(), local)?;
                // Answering is unconditional and stays that way after a path is
                // chosen. A peer that stops seeing answers withdraws the path,
                // and on a relayed path it withholds media entirely.
                Ok(Inbound::CheckAnswered)
            }
            Method::BindingSuccess => {
                let tid = message.transaction_id();

                // A reflexive answer carries no credentials, so the only thing
                // admitting it is that we are still expecting this transaction
                // from this address. The identifier is derived from a seed the
                // sender does not have, which is what makes that sufficient.
                if let Some(server) = self
                    .servers
                    .iter_mut()
                    .flatten()
                    .find(|s| s.sent.contains(tid) && s.addr == from)
                {
                    server.answered = true;
                    return match message.mapped_address() {
                        Some(mapped) => {
                            server.mapped = Some(mapped);
                            Ok(Inbound::Reflexive(mapped))
                        }
                        None => Err(Error::Malformed),
                    };
                }

                if !message.verify(self.credentials.remote_pwd) {
                    return Err(Error::Decrypt);
                }
                let known = self
                    .candidates
                    .iter_mut()
                    .flatten()
                    .find(|c| c.sent.contains(tid));
                let Some(candidate) = known else {
                    // An answer to a transaction this attempt never sent.
                    // Authenticated, so it is a duplicate or a replay rather
                    // than an error.
                    return Ok(Inbound::Redundant);
                };
                let addr = candidate.addr;

                if matches!(self.state, State::Checking) {
                    // The first candidate to answer wins, and nothing looks for
                    // a better path afterwards: switching mid-stream costs more
                    // than the improvement is worth.
                    //
                    // The address the winning answer arrived at is latched with
                    // it: the peer proved this exact address pair, and every
                    // later send keeps it rather than letting the routing
                    // table move the source mid-session.
                    self.state = State::Established(addr);
                    self.local = local;
                    Ok(Inbound::PathEstablished(addr))
                } else {
                    Ok(Inbound::Redundant)
                }
            }
        }
    }

    /// Housekeeping. Closes the window when it expires.
    pub fn poll(&mut self, now_ms: f64) {
        if !matches!(self.state, State::Checking) {
            return;
        }
        if now_ms - self.started_ms >= PUNCH_WINDOW_MS {
            self.state = State::Failed(if self.candidate_count() == 0 {
                Failure::NoCandidates
            } else {
                Failure::ProbeTimeout
            });
        }
    }

    /// Milliseconds until this attempt next needs attention.
    ///
    /// The shell arms its wait from this alongside the session's own timer, and
    /// waits for whichever is sooner.
    pub fn next_timer_ms(&self, now_ms: f64) -> f64 {
        // An answer is owed in every state: a peer keeps checking the path it
        // chose for as long as it uses it, and one that goes unanswered
        // concludes the path is gone.
        let owed = self.pending.iter().flatten().next().is_some();
        if !matches!(self.state, State::Checking) {
            return if owed {
                self.pace_wait(now_ms)
            } else {
                f64::INFINITY
            };
        }

        let mut soonest = (self.started_ms + PUNCH_WINDOW_MS - now_ms).max(0.0);
        if owed {
            soonest = soonest.min(self.pace_wait(now_ms));
        }
        for candidate in self.candidates.iter().flatten() {
            if !self.checkable(candidate.kind) {
                continue;
            }
            let due = match candidate.last_check_ms {
                Some(last) => (last + CHECK_CADENCE_MS - now_ms).max(0.0),
                None => 0.0,
            };
            soonest = soonest.min(due.max(self.pace_wait(now_ms)));
        }
        for server in self.servers.iter().flatten().filter(|s| !s.answered) {
            let due = match server.last_probe_ms {
                Some(last) => (last + CHECK_CADENCE_MS - now_ms).max(0.0),
                None => 0.0,
            };
            soonest = soonest.min(due.max(self.pace_wait(now_ms)));
        }
        soonest
    }

    /// Emit the next datagram. Drive until `None`.
    pub fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<Egress>> {
        if self.pace_wait(now_ms) > 0.0 {
            return None;
        }

        // Owed responses go first. A peer waiting on one is being told we are
        // unreachable for as long as it waits.
        if let Some(slot) = self.pending.iter_mut().find(|slot| slot.is_some()) {
            let (to, tid, local) = (*slot)?;
            *slot = None;
            return Some(self.emit_response(to, tid, local, now_ms, out));
        }

        if !matches!(self.state, State::Checking) {
            return None;
        }

        // One probe per attempt, toward the peer's reflexive candidate alone.
        // It exists to open our own mapping on the path that crosses
        // translation, ahead of anything full-length; a direct candidate
        // needs no mapping opened, so it never draws the probe, and the latch
        // waits for a reflexive candidate to exist rather than spending the
        // one probe on whichever address arrived first.
        if !self.probe_sent
            && let Some(to) = self
                .candidates
                .iter()
                .flatten()
                .find(|c| c.kind == Kind::Reflexive)
                .map(|c| c.addr)
        {
            self.probe_sent = true;
            return Some(self.emit_check(to, Ttl::Probe, now_ms, out));
        }

        // Learning our own address is worth doing early, so it outranks peer
        // checks: a candidate we have not discovered cannot be offered, and the
        // peer cannot check what it was never told about.
        let server_due = self.servers.iter().enumerate().find_map(|(index, slot)| {
            let server = slot.as_ref()?;
            let ready = !server.answered
                && match server.last_probe_ms {
                    Some(last) => now_ms - last >= CHECK_CADENCE_MS,
                    None => true,
                };
            ready.then_some((index, server.addr))
        });
        if let Some((index, to)) = server_due {
            return Some(self.emit_reflexive(index, to, now_ms, out));
        }

        let due = self
            .candidates
            .iter()
            .enumerate()
            .find_map(|(index, slot)| {
                let candidate = slot.as_ref()?;
                if !self.checkable(candidate.kind) {
                    return None;
                }
                let ready = match candidate.last_check_ms {
                    Some(last) => now_ms - last >= CHECK_CADENCE_MS,
                    None => true,
                };
                ready.then_some((index, candidate.addr))
            })?;

        let (index, to) = due;
        let result = self.emit_check(to, Ttl::Default, now_ms, out);
        if let Ok(egress) = &result
            && let Some(candidate) = self.candidates.get_mut(index).and_then(Option::as_mut)
        {
            candidate.last_check_ms = Some(now_ms);
            let _ = egress;
        }
        Some(result)
    }

    /// How long until pacing permits another datagram.
    fn pace_wait(&self, now_ms: f64) -> f64 {
        match self.last_sent_ms {
            Some(last) => (last + PACING_MS - now_ms).max(0.0),
            None => 0.0,
        }
    }

    fn queue_response(
        &mut self,
        to: SocketAddr,
        tid: TransactionId,
        local: Option<IpAddr>,
    ) -> Result<()> {
        let slot = self
            .pending
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Oversized)?;
        *slot = Some((to, tid, local));
        Ok(())
    }

    fn emit_check(
        &mut self,
        to: SocketAddr,
        ttl: Ttl,
        now_ms: f64,
        out: &mut [u8],
    ) -> Result<Egress> {
        let tid = self.next_transaction_id();
        let len = stun::encode_binding_request(
            out,
            tid,
            self.credentials.local_ufrag,
            self.credentials.remote_ufrag,
            self.tiebreaker(),
            self.credentials.remote_pwd,
        )?;
        if let Some(candidate) = self.candidates.iter_mut().flatten().find(|c| c.addr == to) {
            candidate.sent.push(tid);
        }
        self.last_sent_ms = Some(now_ms);
        // The latch is None for the whole punch, so checks let the kernel
        // choose; it is carried anyway so a check emitted after a path exists
        // would leave from the proven address like everything else.
        Ok(Egress {
            to,
            ttl,
            len,
            from: self.local,
        })
    }

    fn emit_reflexive(
        &mut self,
        index: usize,
        to: SocketAddr,
        now_ms: f64,
        out: &mut [u8],
    ) -> Result<Egress> {
        let tid = self.next_transaction_id();
        let len = stun::encode_reflexive_request(out, tid)?;
        if let Some(server) = self.servers.get_mut(index).and_then(Option::as_mut) {
            server.last_probe_ms = Some(now_ms);
            server.sent.push(tid);
        }
        self.last_sent_ms = Some(now_ms);
        // Unpinned on purpose: the question is what mapping the default route
        // produces, and servers are never probed once a path exists.
        Ok(Egress {
            to,
            ttl: Ttl::Default,
            len,
            from: None,
        })
    }

    fn emit_response(
        &mut self,
        to: SocketAddr,
        tid: TransactionId,
        local: Option<IpAddr>,
        now_ms: f64,
        out: &mut [u8],
    ) -> Result<Egress> {
        let len = stun::encode_binding_response(out, tid, to, self.credentials.local_pwd)?;
        self.last_sent_ms = Some(now_ms);
        // Answered from the address the check arrived at. The peer's filter
        // admitted exactly that pair; a reply from a sibling address is
        // unsolicited traffic to it, and on a multi-homed host the kernel's
        // default choice is the sibling.
        Ok(Egress {
            to,
            ttl: Ttl::Default,
            len,
            from: local,
        })
    }

    /// Derive the next transaction identifier from the seed and a counter.
    ///
    /// Never random. The identifier is echoed rather than validated and the
    /// integrity attribute is what authenticates, so it only has to be unique
    /// among the transactions this attempt has sent. Deriving it keeps the core free of a
    /// random number generator and makes a failing run replayable from its
    /// seed.
    fn next_transaction_id(&mut self) -> TransactionId {
        let counter = self.counter;
        self.counter = self.counter.wrapping_add(1);
        derive_transaction_id(&self.seed, counter)
    }

    /// Role tiebreaker. Inert, because the role is fixed and no conflict can
    /// arise, so it is derived rather than generated for the same reason as the
    /// transaction identifier.
    fn tiebreaker(&self) -> [u8; 8] {
        let mut value = [0u8; 8];
        for (slot, byte) in value.iter_mut().zip(self.seed.iter()) {
            *slot = *byte;
        }
        value
    }
}

/// The identifier for the `counter`th transaction under `seed`: the first
/// twelve bytes of a digest of the two.
pub(crate) fn derive_transaction_id(seed: &[u8; 16], counter: u32) -> TransactionId {
    let mut hash = Sha1::new();
    hash.update(seed);
    hash.update(counter.to_be_bytes());
    let digest = hash.finalize();

    let mut tid = [0u8; 12];
    for (slot, byte) in tid.iter_mut().zip(digest.iter()) {
        *slot = *byte;
    }
    TransactionId(tid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{IpAddr, Ipv4Addr};

    const SEED: [u8; 16] = [0x11; 16];
    const OURS: &str = "loca";
    const OUR_PWD: &str = "localpassword";
    const THEIRS: &str = "remo";
    const THEIR_PWD: &str = "remotepassword";

    fn credentials() -> Credentials<'static> {
        Credentials {
            local_ufrag: OURS,
            local_pwd: OUR_PWD,
            remote_ufrag: THEIRS,
            remote_pwd: THEIR_PWD,
        }
    }

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, last)), port)
    }

    fn conn() -> Conn<'static> {
        Conn::new(credentials(), SEED, 0.0)
    }

    /// Drain every datagram the engine will emit at `now`, advancing time by
    /// the pacing interval so the drain is not cut short by it.
    fn drain(conn: &mut Conn<'_>, now: f64) -> std::vec::Vec<(Egress, [u8; 256])> {
        let mut out = std::vec::Vec::new();
        let mut at = now;
        loop {
            let mut buf = [0u8; 256];
            match conn.get_output(at, &mut buf) {
                Some(Ok(egress)) => {
                    out.push((egress, buf));
                    at += PACING_MS;
                }
                Some(Err(error)) => panic!("emit failed: {error}"),
                None => return out,
            }
        }
    }

    /// The peer's side of a check: verify what we sent, and answer it.
    fn answer(request: &[u8], len: usize, from: SocketAddr, out: &mut [u8]) -> usize {
        let message = Message::parse(&request[..len]).unwrap();
        assert_eq!(message.method(), Method::BindingRequest);
        assert!(
            message.verify(THEIR_PWD),
            "a check must be signed with the peer's password"
        );
        assert_eq!(message.username(), Some("remo:loca"));
        stun::encode_binding_response(out, message.transaction_id(), from, THEIR_PWD).unwrap()
    }

    #[test]
    fn the_first_datagram_is_a_probe_and_only_the_first() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();
        conn.add_candidate(addr(2, 4000), Kind::Reflexive).unwrap();

        let sent = drain(&mut conn, 0.0);
        assert_eq!(sent.first().map(|(e, _)| e.ttl), Some(Ttl::Probe));
        assert_eq!(
            sent.iter().filter(|(e, _)| e.ttl == Ttl::Probe).count(),
            1,
            "the probe is once per attempt, not once per candidate"
        );
        assert!(
            sent.iter().skip(1).all(|(e, _)| e.ttl == Ttl::Default),
            "a check after the probe must go out at the normal TTL"
        );
    }

    /// The probe opens our mapping on the path that crosses translation, so
    /// its target is the peer's reflexive candidate -- whichever position it
    /// arrived in. A direct candidate needs no mapping opened, and a probe
    /// spent on it leaves the crossing path unopened.
    #[test]
    fn the_probe_goes_to_a_reflexive_candidate_not_the_first() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Direct).unwrap();
        conn.add_candidate(addr(2, 4000), Kind::Reflexive).unwrap();

        let sent = drain(&mut conn, 0.0);
        let probe = sent
            .iter()
            .find(|(egress, _)| egress.ttl == Ttl::Probe)
            .expect("no probe went out");
        assert_eq!(
            probe.0.to,
            addr(2, 4000),
            "the probe went to a direct candidate"
        );
    }

    /// No reflexive candidate, no probe -- and the latch waits for a target
    /// to exist rather than for the first arrival, so one turning up late
    /// still draws it.
    #[test]
    fn the_probe_waits_for_a_reflexive_candidate() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Direct).unwrap();

        let sent = drain(&mut conn, 0.0);
        assert!(
            sent.iter().all(|(egress, _)| egress.ttl == Ttl::Default),
            "a direct candidate drew the probe"
        );

        conn.add_candidate(addr(2, 4000), Kind::Reflexive).unwrap();
        let sent = drain(&mut conn, 600.0);
        assert!(
            sent.iter()
                .any(|(egress, _)| egress.ttl == Ttl::Probe && egress.to == addr(2, 4000)),
            "a late reflexive candidate never drew the probe"
        );
    }

    /// The exchange's flags map onto the three kinds, lan winning; a
    /// translated-path guess no server verified waits for the readiness
    /// marker like the reflexive path, and never draws the one probe.
    #[test]
    fn a_wan_guess_is_checked_but_never_probed() {
        assert_eq!(Kind::marked(true, false), Kind::Direct);
        assert_eq!(Kind::marked(false, true), Kind::Reflexive);
        assert_eq!(Kind::marked(false, false), Kind::Wan);
        assert_eq!(Kind::marked(true, true), Kind::Direct, "lan must win");

        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Wan).unwrap();
        assert!(
            drain(&mut conn, 0.0).is_empty(),
            "a translated-path guess was checked before the peer said it listens"
        );

        conn.set_peer_ready();
        let sent = drain(&mut conn, 20.0);
        assert!(
            !sent.is_empty(),
            "a wan guess must be checked once the peer is ready"
        );
        assert!(
            sent.iter().all(|(egress, _)| egress.ttl == Ttl::Default),
            "the probe was spent on an unverified guess"
        );
    }

    /// Full-length checks toward translated paths wait for the readiness
    /// marker; the probe does not, because it never reaches the peer. A
    /// check that arrives before the peer has sent anything outward is
    /// unsolicited traffic to its translator, which can commit a state entry
    /// that then blocks the peer's own punch.
    #[test]
    fn wan_checks_wait_for_the_readiness_marker() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();

        let sent = drain(&mut conn, 0.0);
        assert_eq!(sent.len(), 1, "only the probe may leave before readiness");
        assert_eq!(sent.first().map(|(egress, _)| egress.ttl), Some(Ttl::Probe));

        // And the gated candidate does not drive the timer to zero, or the
        // loop would spin on a deadline it is not allowed to act on.
        let wait = conn.next_timer_ms(20.0);
        assert!(wait > 400.0, "a gated candidate armed the timer: {wait}");

        conn.set_peer_ready();
        assert!(
            conn.next_timer_ms(20.0).abs() < 1e-9,
            "readiness must make the candidate due"
        );
        let sent = drain(&mut conn, 20.0);
        assert!(
            sent.iter().any(|(egress, _)| egress.ttl == Ttl::Default),
            "no check followed the readiness marker"
        );
    }

    /// A direct candidate never waits: it is reachable as given and there is
    /// no translator on its path to poison.
    #[test]
    fn direct_candidates_never_wait() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Direct).unwrap();
        let sent = drain(&mut conn, 0.0);
        assert!(
            sent.iter().any(|(egress, _)| egress.ttl == Ttl::Default),
            "a direct candidate was held for a marker it does not need"
        );
    }

    /// The regression for the restore obligation. A probe is emitted at a TTL
    /// that cannot reach the peer, and every datagram after it must be back at
    /// the default; a shell that never restored would show as a path that
    /// establishes and then carries nothing over any distance.
    #[test]
    fn only_the_probe_carries_the_reduced_ttl() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();
        conn.set_peer_ready();

        let mut ttls = std::vec::Vec::new();
        let mut at = 0.0;
        while at < 3_000.0 {
            for (egress, _) in drain(&mut conn, at) {
                ttls.push(egress.ttl);
            }
            at += CHECK_CADENCE_MS;
            conn.poll(at);
        }

        assert_eq!(ttls.first(), Some(&Ttl::Probe));
        assert!(
            ttls.len() > 3,
            "expected repeated checks, got {}",
            ttls.len()
        );
        assert!(
            ttls.iter().skip(1).all(|ttl| *ttl == Ttl::Default),
            "a reduced TTL escaped past the probe: {ttls:?}"
        );
    }

    #[test]
    fn a_candidate_that_answers_becomes_the_path() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        conn.add_candidate(peer, Kind::Reflexive).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);

        assert_eq!(
            conn.process_input(&response[..len], peer, None).unwrap(),
            Inbound::PathEstablished(peer)
        );
        assert_eq!(conn.state(), State::Established(peer));
        assert_eq!(conn.path(), Some(peer));
    }

    #[test]
    fn checks_stop_once_a_path_is_chosen() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        conn.add_candidate(peer, Kind::Reflexive).unwrap();
        conn.add_candidate(addr(2, 4000), Kind::Reflexive).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);
        conn.process_input(&response[..len], peer, None).unwrap();

        assert!(
            drain(&mut conn, 600.0).is_empty(),
            "kept probing after the path was chosen"
        );
    }

    /// Answering is unconditional and outlives path selection. A peer that
    /// stops seeing answers treats us as unreachable even while media flows.
    #[test]
    fn inbound_checks_are_answered_even_after_the_path_is_chosen() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        conn.add_candidate(peer, Kind::Reflexive).unwrap();
        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);
        conn.process_input(&response[..len], peer, None).unwrap();

        // Now the peer checks us. It signs with our password, because from its
        // side we are the remote.
        let mut theirs = [0u8; 256];
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5A; 12]),
            THEIRS,
            OURS,
            [0; 8],
            OUR_PWD,
        )
        .unwrap();

        assert_eq!(
            conn.process_input(&theirs[..len], peer, None).unwrap(),
            Inbound::CheckAnswered
        );
        let sent = drain(&mut conn, 700.0);
        assert_eq!(sent.len(), 1, "the answer did not go out");

        let (egress, buf) = sent.first().copied().unwrap();
        let reply = Message::parse(&buf[..egress.len]).unwrap();
        assert_eq!(reply.method(), Method::BindingSuccess);
        assert!(
            reply.verify(OUR_PWD),
            "a response must be signed with our own password"
        );
        assert_eq!(reply.mapped_address(), Some(peer));
    }

    /// A peer that checks after the path is chosen is owed an answer on the
    /// clock, not on the next wake something else causes. A timer that reads
    /// infinity once established leaves that answer to whatever wakes the
    /// loop next, which on a quiet link is the peer giving up.
    #[test]
    fn a_pending_answer_arms_the_timer_after_establishment() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        conn.add_candidate(peer, Kind::Reflexive).unwrap();
        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);
        conn.process_input(&response[..len], peer, None).unwrap();
        assert!(conn.next_timer_ms(600.0).is_infinite());

        let mut theirs = [0u8; 256];
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5B; 12]),
            THEIRS,
            OURS,
            [0; 8],
            OUR_PWD,
        )
        .unwrap();
        conn.process_input(&theirs[..len], peer, None).unwrap();

        let due = conn.next_timer_ms(600.0);
        assert!(due.is_finite() && due <= PACING_MS, "timer reads {due}");
        assert_eq!(drain(&mut conn, 600.0 + due).len(), 1);
        assert!(conn.next_timer_ms(700.0).is_infinite());
    }

    /// A peer running a full agent checks every pair it holds in one burst.
    /// Every one of those is owed an answer; one dropped for want of a slot
    /// reads to the peer as a pair that failed.
    #[test]
    fn sixteen_checks_in_one_burst_are_all_answered() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        for index in 0..16u8 {
            let mut theirs = [0u8; 256];
            let len = stun::encode_binding_request(
                &mut theirs,
                TransactionId([index; 12]),
                THEIRS,
                OURS,
                [0; 8],
                OUR_PWD,
            )
            .unwrap();
            assert_eq!(
                conn.process_input(&theirs[..len], peer, None).unwrap(),
                Inbound::CheckAnswered,
                "check {index}"
            );
        }

        let sent = drain(&mut conn, 0.0);
        let answers = sent
            .iter()
            .filter(|(egress, buf)| {
                Message::parse(&buf[..egress.len]).unwrap().method() == Method::BindingSuccess
            })
            .count();
        assert_eq!(answers, 16, "answers went out for {answers} of 16 checks");
    }

    /// An answer that took longer than the check cadence is still an answer.
    /// Every identifier this attempt has asked a candidate with stays valid
    /// for the attempt's life: matched against only the newest, a path whose
    /// round trip exceeds the cadence fails deterministically, the answer
    /// forever one identifier behind.
    #[test]
    fn an_answer_slower_than_the_check_cadence_still_establishes() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        conn.add_candidate(peer, Kind::Reflexive).unwrap();
        conn.set_peer_ready();

        // The first round leaves, and the peer's answer to it is built now
        // but will arrive late.
        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);

        // The cadence re-checks before the answer lands. The drain helper
        // paces its emissions, so the first check left a pace step after the
        // probe; ask comfortably past one cadence from there.
        let again = drain(&mut conn, CHECK_CADENCE_MS + 2.0 * PACING_MS);
        assert!(!again.is_empty(), "no re-check went out, fixture broken");

        assert_eq!(
            conn.process_input(&response[..len], peer, None).unwrap(),
            Inbound::PathEstablished(peer),
            "a true answer one identifier old was discarded"
        );
    }

    /// The same rule for a reflexive server: its report is matched against
    /// every probe this attempt sent it, not only the newest.
    #[test]
    fn a_server_answer_slower_than_the_cadence_still_teaches() {
        let mut conn = conn();
        let server = addr(50, 3478);
        let observed = addr(9, 41_000);
        conn.add_server(server).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.first().copied().unwrap();
        let tid = Message::parse(&buf[..egress.len]).unwrap().transaction_id();

        let again = drain(&mut conn, CHECK_CADENCE_MS + 1.0);
        assert!(!again.is_empty(), "no re-probe went out, fixture broken");

        let mut response = [0u8; 256];
        let len = stun::encode_binding_response(&mut response, tid, observed, "any").unwrap();
        assert_eq!(
            conn.process_input(&response[..len], server, None).unwrap(),
            Inbound::Reflexive(observed),
            "a true report one identifier old was refused"
        );
    }

    /// A check is answered from the address it arrived at. The peer's filter
    /// admitted exactly that pair; on a multi-homed host the kernel's default
    /// pick is the primary sibling, and a reply from it is unsolicited traffic
    /// the peer never sees -- the one candidate the second address exists for
    /// then never completes a check.
    #[test]
    fn a_response_leaves_from_the_address_the_check_arrived_at() {
        let mut conn = conn();
        let arrived_at = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));
        let mut theirs = [0u8; 256];
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5A; 12]),
            THEIRS,
            OURS,
            [0; 8],
            OUR_PWD,
        )
        .unwrap();
        conn.process_input(&theirs[..len], addr(7, 41_000), Some(arrived_at))
            .unwrap();

        let sent = drain(&mut conn, 0.0);
        let (response, _) = sent
            .iter()
            .copied()
            .find(|(egress, _)| egress.to == addr(7, 41_000) && egress.ttl == Ttl::Default)
            .expect("the answer");
        assert_eq!(
            response.from,
            Some(arrived_at),
            "the answer must leave from the address the check arrived at"
        );

        // And a shell that could not say leaves the choice to the kernel.
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5B; 12]),
            THEIRS,
            OURS,
            [0; 8],
            OUR_PWD,
        )
        .unwrap();
        conn.process_input(&theirs[..len], addr(7, 41_000), None)
            .unwrap();
        let sent = drain(&mut conn, 100.0);
        let answer = sent
            .iter()
            .find(|(egress, _)| egress.to == addr(7, 41_000) && egress.ttl == Ttl::Default)
            .expect("the answer");
        assert_eq!(answer.0.from, None);
    }

    /// The winning answer's arrival address is latched with the path, and it
    /// is what [`Conn::local`] reports from then on. Checks and probes before
    /// it leave unpinned, because nothing is proven yet.
    #[test]
    fn the_winning_answer_latches_the_local_address() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        let proven = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3));
        conn.add_candidate(peer, Kind::Reflexive).unwrap();

        let sent = drain(&mut conn, 0.0);
        assert!(
            sent.iter().all(|(egress, _)| egress.from.is_none()),
            "a check before the path exists must not claim a source"
        );
        assert_eq!(conn.local(), None);

        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);
        conn.process_input(&response[..len], peer, Some(proven))
            .unwrap();

        assert_eq!(conn.state(), State::Established(peer));
        assert_eq!(
            conn.local(),
            Some(proven),
            "the address the winning answer arrived at was not latched"
        );
    }

    /// The regression for a real wide-area failure. A peer behind symmetric
    /// translation is reachable only at the address its checks actually come
    /// from: what it advertised was created toward a reflexive server and its
    /// packets to us leave from a different mapping. Without this the peer
    /// connects from its side while we never find a path.
    #[test]
    fn a_verified_check_teaches_us_the_address_it_came_from() {
        let mut conn = conn();
        assert_eq!(conn.candidate_count(), 0);

        let observed = addr(7, 41_000);
        let mut theirs = [0u8; 256];
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5A; 12]),
            THEIRS,
            OURS,
            [0; 8],
            OUR_PWD,
        )
        .unwrap();
        conn.process_input(&theirs[..len], observed, None).unwrap();

        assert_eq!(
            conn.candidate_count(),
            1,
            "the source of a verified check must become a candidate"
        );

        // Stored is not enough; it has to be checked, or we still never
        // establish.
        let sent = drain(&mut conn, 0.0);
        assert!(
            sent.iter().any(|(egress, _)| egress.to == observed),
            "the learned candidate was never checked"
        );
    }

    /// And authentication is the whole of the admission. An unauthenticated
    /// source address would let anyone able to reach the socket point us
    /// anywhere.
    #[test]
    fn an_unverified_check_teaches_us_nothing() {
        let mut conn = conn();
        let mut theirs = [0u8; 256];
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5A; 12]),
            THEIRS,
            OURS,
            [0; 8],
            "not the password",
        )
        .unwrap();

        assert_eq!(
            conn.process_input(&theirs[..len], addr(7, 41_000), None),
            Err(Error::Decrypt)
        );
        assert_eq!(
            conn.candidate_count(),
            0,
            "an unauthenticated source must not become a candidate"
        );
    }

    #[test]
    fn a_check_signed_with_the_wrong_password_is_refused() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        let mut theirs = [0u8; 256];
        let len = stun::encode_binding_request(
            &mut theirs,
            TransactionId([0x5A; 12]),
            THEIRS,
            OURS,
            [0; 8],
            "not the password",
        )
        .unwrap();

        assert_eq!(
            conn.process_input(&theirs[..len], peer, None),
            Err(Error::Decrypt)
        );
        assert!(
            drain(&mut conn, 1.0).is_empty(),
            "answered a check that failed authentication"
        );
    }

    /// **Two servers, two answers, and both are kept.**
    ///
    /// This is what more than one reflexive server is for: a translator that
    /// maps endpoint-independently reports one address to both, and a symmetric
    /// one reports a different port to each. Keeping only the newest would
    /// erase exactly the difference that distinguishes them, and would send the
    /// peer one candidate where two were learned.
    ///
    /// Every other test here uses one server, so the whole multi-server path --
    /// four slots, a probe per server, an answer retained per server -- ran
    /// unexercised until this.
    #[test]
    fn two_servers_that_see_us_differently_both_have_their_say() {
        let mut conn = conn();
        let first = addr(50, 3478);
        let second = addr(51, 3478);
        conn.add_server(first).unwrap();
        conn.add_server(second).unwrap();

        // One probe each, not one between them.
        let sent = drain(&mut conn, 0.0);
        let probed = |to: SocketAddr| sent.iter().any(|(egress, _)| egress.to == to);
        assert!(
            probed(first) && probed(second),
            "one probe went out where two servers were added"
        );

        // A symmetric translator gives each server a different port.
        let seen_by_first = addr(9, 41_000);
        let seen_by_second = addr(9, 41_001);
        for (server, seen) in [(first, seen_by_first), (second, seen_by_second)] {
            let (egress, buf) = sent
                .iter()
                .copied()
                .find(|(egress, _)| egress.to == server)
                .expect("a probe for this server");
            let request = Message::parse(&buf[..egress.len]).unwrap();
            let mut response = [0u8; 256];
            let len =
                stun::encode_binding_response(&mut response, request.transaction_id(), seen, "any")
                    .unwrap();
            assert_eq!(
                conn.process_input(&response[..len], server, None).unwrap(),
                Inbound::Reflexive(seen)
            );
        }

        let learned = |addr: SocketAddr| conn.reflexive().any(|seen| seen == addr);
        assert!(
            learned(seen_by_first) && learned(seen_by_second),
            "one server's answer overwrote the other's"
        );

        // And an answered server is not probed again, or every session would
        // keep asking a question it already has the answer to.
        let again = drain(&mut conn, CHECK_CADENCE_MS * 2.0);
        assert!(
            !again
                .iter()
                .any(|(egress, _)| egress.to == first || egress.to == second),
            "a server that already answered was probed again"
        );
    }

    #[test]
    fn a_reflexive_server_teaches_us_our_own_address() {
        let mut conn = conn();
        let server = addr(50, 3478);
        let observed = addr(9, 41_000);
        conn.add_server(server).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.first().copied().unwrap();
        assert_eq!(egress.to, server);
        assert_eq!(egress.ttl, Ttl::Default);

        // A public server answers without credentials of any kind.
        let request = Message::parse(&buf[..egress.len]).unwrap();
        assert!(
            !request.is_authenticated(),
            "a reflexive probe must carry no credentials"
        );
        let mut response = [0u8; 256];
        let len =
            stun::encode_binding_response(&mut response, request.transaction_id(), observed, "any")
                .unwrap();

        assert_eq!(
            conn.process_input(&response[..len], server, None).unwrap(),
            Inbound::Reflexive(observed)
        );
    }

    /// The one-shot report is not enough on its own.
    ///
    /// A caller that processes datagrams in batches -- which the shell does --
    /// has nowhere to put a per-datagram return value, so a reflexive candidate
    /// reported only that way is learned and immediately lost. It is the
    /// candidate a wide-area path depends on, so it is retained and asked for.
    #[test]
    fn a_reflexive_candidate_is_retained_after_it_is_reported() {
        let mut conn = conn();
        let server = addr(50, 3478);
        let observed = addr(9, 41_000);
        conn.add_server(server).unwrap();

        assert_eq!(
            conn.reflexive().count(),
            0,
            "nothing is known before an answer"
        );

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.first().copied().unwrap();
        let request = Message::parse(&buf[..egress.len]).unwrap();
        let mut response = [0u8; 256];
        let len =
            stun::encode_binding_response(&mut response, request.transaction_id(), observed, "any")
                .unwrap();
        conn.process_input(&response[..len], server, None).unwrap();

        let mut gathered = conn.reflexive();
        assert_eq!(
            gathered.next(),
            Some(observed),
            "the candidate must survive the call that reported it"
        );
        assert_eq!(gathered.next(), None);
    }

    /// The transaction identifier is the whole of the admission check for an
    /// unauthenticated answer, so one we never sent must be refused. Otherwise
    /// anyone able to reach the socket could dictate the address we advertise.
    #[test]
    fn a_reflexive_answer_we_did_not_ask_for_is_refused() {
        let mut conn = conn();
        let server = addr(50, 3478);
        conn.add_server(server).unwrap();
        drain(&mut conn, 0.0);

        let mut response = [0u8; 256];
        let len = stun::encode_binding_response(
            &mut response,
            TransactionId([0xEE; 12]),
            addr(9, 41_000),
            "any",
        )
        .unwrap();

        assert_eq!(
            conn.process_input(&response[..len], server, None),
            Err(Error::Decrypt),
            "an unexpected transaction must not set our advertised address"
        );
    }

    /// And one with the right identifier from the wrong place is refused too.
    #[test]
    fn a_reflexive_answer_from_the_wrong_address_is_refused() {
        let mut conn = conn();
        let server = addr(50, 3478);
        conn.add_server(server).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.first().copied().unwrap();
        let tid = Message::parse(&buf[..egress.len]).unwrap().transaction_id();

        let mut response = [0u8; 256];
        let len =
            stun::encode_binding_response(&mut response, tid, addr(9, 41_000), "any").unwrap();

        assert_eq!(
            conn.process_input(&response[..len], addr(66, 1234), None),
            Err(Error::Decrypt)
        );
    }

    #[test]
    fn a_server_stops_being_probed_once_it_answers() {
        let mut conn = conn();
        let server = addr(50, 3478);
        conn.add_server(server).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.first().copied().unwrap();
        let tid = Message::parse(&buf[..egress.len]).unwrap().transaction_id();
        let mut response = [0u8; 256];
        let len =
            stun::encode_binding_response(&mut response, tid, addr(9, 41_000), "any").unwrap();
        conn.process_input(&response[..len], server, None).unwrap();

        assert!(
            drain(&mut conn, CHECK_CADENCE_MS + 1.0).is_empty(),
            "kept probing a server that already answered"
        );
    }

    #[test]
    fn the_window_closes_with_a_typed_failure() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();

        conn.poll(PUNCH_WINDOW_MS - 1.0);
        assert_eq!(conn.state(), State::Checking);

        conn.poll(PUNCH_WINDOW_MS);
        assert_eq!(conn.state(), State::Failed(Failure::ProbeTimeout));
        assert!(drain(&mut conn, PUNCH_WINDOW_MS).is_empty());
    }

    #[test]
    fn an_attempt_with_nothing_to_check_says_so() {
        let mut conn = conn();
        conn.poll(PUNCH_WINDOW_MS);
        assert_eq!(conn.state(), State::Failed(Failure::NoCandidates));
    }

    #[test]
    fn a_duplicate_candidate_is_ignored_and_a_full_table_refuses() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();
        assert_eq!(conn.candidate_count(), 1);

        for index in 0..MAX_CANDIDATES {
            let _ = conn.add_candidate(addr(2, 4000 + index as u16), Kind::Reflexive);
        }
        assert_eq!(conn.candidate_count(), MAX_CANDIDATES);
        assert_eq!(
            conn.add_candidate(addr(3, 9999), Kind::Reflexive),
            Err(Error::Oversized),
            "a full table must refuse rather than evict"
        );
    }

    /// A v4-mapped candidate is the same candidate. Admitting both would spend
    /// two slots and two check budgets on one address.
    #[test]
    fn a_v4_mapped_candidate_is_not_a_second_candidate() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();
        conn.add_candidate(
            "[::ffff:198.51.100.1]:4000".parse().unwrap(),
            Kind::Reflexive,
        )
        .unwrap();
        assert_eq!(conn.candidate_count(), 1);
    }

    #[test]
    fn transaction_identifiers_do_not_repeat_and_follow_the_seed() {
        let mut left = conn();
        let mut right = conn();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..64 {
            let a = left.next_transaction_id();
            let b = right.next_transaction_id();
            assert_eq!(a, b, "the same seed must produce the same run");
            seen.insert(a.0);
        }
        assert_eq!(seen.len(), 64, "a transaction identifier repeated");

        let mut other = Conn::new(credentials(), [0x22; 16], 0.0);
        assert_ne!(
            other.next_transaction_id(),
            Conn::new(credentials(), SEED, 0.0).next_transaction_id(),
            "a different seed must produce a different run"
        );
    }

    #[test]
    fn the_timer_tracks_the_cadence_and_the_window() {
        let mut conn = conn();
        assert!(conn.next_timer_ms(0.0).is_finite());

        conn.add_candidate(addr(1, 4000), Kind::Reflexive).unwrap();
        conn.set_peer_ready();
        assert!(
            conn.next_timer_ms(0.0).abs() < 1e-9,
            "a fresh candidate is due immediately"
        );

        drain(&mut conn, 0.0);
        let wait = conn.next_timer_ms(20.0);
        assert!(wait > 0.0 && wait <= CHECK_CADENCE_MS, "{wait}");

        conn.poll(PUNCH_WINDOW_MS);
        assert!(conn.next_timer_ms(PUNCH_WINDOW_MS).is_infinite());
    }
}
