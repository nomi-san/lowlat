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
//! once     one probe at a TTL too low to leave the local network
//! every    500 ms per candidate, an authenticated check
//! t=7500   no answer, the attempt is over
//! ```
//!
//! Fifteen checks per candidate is the entire budget. There is no slow retry
//! tier behind it, which is why a candidate that cannot possibly answer must
//! never be admitted in the first place.

use core::net::SocketAddr;

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
const MAX_PENDING: usize = 4;

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

#[derive(Debug, Clone, Copy)]
struct Candidate {
    addr: SocketAddr,
    /// When a check was last emitted, or `None` if none ever was.
    last_check_ms: Option<f64>,
    /// Identifier of the outstanding check, matched against a response.
    outstanding: Option<TransactionId>,
}

#[derive(Debug, Clone, Copy)]
struct Server {
    addr: SocketAddr,
    last_probe_ms: Option<f64>,
    /// Identifier of the outstanding probe. A server answer carries no
    /// credentials, so this is the only thing that admits one.
    outstanding: Option<TransactionId>,
    answered: bool,
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
    pending: [Option<(SocketAddr, TransactionId)>; MAX_PENDING],

    state: State,
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

    /// The chosen path, once there is one.
    pub fn path(&self) -> Option<SocketAddr> {
        match self.state {
            State::Established(addr) => Some(addr),
            _ => None,
        }
    }

    /// Offer a remote candidate.
    ///
    /// Candidates trickle in as the peer discovers them, so this is called
    /// repeatedly and at any time. A duplicate is ignored and a full table
    /// drops the arrival rather than evicting something already being checked.
    pub fn add_candidate(&mut self, addr: SocketAddr) -> Result<()> {
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
            last_check_ms: None,
            outstanding: None,
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
            outstanding: None,
            answered: false,
        });
        Ok(())
    }

    /// Feed one datagram that classified as a connectivity check.
    ///
    /// `from` is the address it actually arrived from, which is the only
    /// trustworthy source: a peer behind address translation cannot know it.
    pub fn process_input(&mut self, datagram: &[u8], from: SocketAddr) -> Result<Inbound> {
        let from = stun::canonical(from);
        let message = Message::parse(datagram)?;

        match message.method() {
            Method::BindingRequest => {
                // Signed with our password, because from the peer's side we are
                // the remote. A request that fails is dropped without a
                // response rather than answered with an error.
                if !message.verify(self.credentials.local_pwd) {
                    return Err(Error::Decrypt);
                }
                self.queue_response(from, message.transaction_id())?;
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
                    .find(|s| s.outstanding == Some(tid) && s.addr == from)
                {
                    server.outstanding = None;
                    server.answered = true;
                    return match message.mapped_address() {
                        Some(mapped) => Ok(Inbound::Reflexive(mapped)),
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
                    .find(|c| c.outstanding == Some(tid));
                let Some(candidate) = known else {
                    // A late answer to a transaction we have moved past. Not an
                    // error; it is a race with the cadence.
                    return Ok(Inbound::Redundant);
                };
                candidate.outstanding = None;
                let addr = candidate.addr;

                if matches!(self.state, State::Checking) {
                    // The first candidate to answer wins, and nothing looks for
                    // a better path afterwards: switching mid-stream costs more
                    // than the improvement is worth.
                    self.state = State::Established(addr);
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
        if !matches!(self.state, State::Checking) {
            return f64::INFINITY;
        }

        let mut soonest = (self.started_ms + PUNCH_WINDOW_MS - now_ms).max(0.0);
        if self.pending.iter().flatten().next().is_some() {
            soonest = soonest.min(self.pace_wait(now_ms));
        }
        for candidate in self.candidates.iter().flatten() {
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
            let (to, tid) = (*slot)?;
            *slot = None;
            return Some(self.emit_response(to, tid, now_ms, out));
        }

        if !matches!(self.state, State::Checking) {
            return None;
        }

        // One probe per attempt, at the first candidate to arrive. It exists to
        // open the local mapping, not to reach anyone, so repeating it per
        // candidate would buy nothing and cost budget.
        if !self.probe_sent
            && let Some(to) = self.candidates.iter().flatten().next().map(|c| c.addr)
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

    fn queue_response(&mut self, to: SocketAddr, tid: TransactionId) -> Result<()> {
        let slot = self
            .pending
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::Oversized)?;
        *slot = Some((to, tid));
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
            candidate.outstanding = Some(tid);
        }
        self.last_sent_ms = Some(now_ms);
        Ok(Egress { to, ttl, len })
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
            server.outstanding = Some(tid);
        }
        self.last_sent_ms = Some(now_ms);
        Ok(Egress {
            to,
            ttl: Ttl::Default,
            len,
        })
    }

    fn emit_response(
        &mut self,
        to: SocketAddr,
        tid: TransactionId,
        now_ms: f64,
        out: &mut [u8],
    ) -> Result<Egress> {
        let len = stun::encode_binding_response(out, tid, to, self.credentials.local_pwd)?;
        self.last_sent_ms = Some(now_ms);
        Ok(Egress {
            to,
            ttl: Ttl::Default,
            len,
        })
    }

    /// Derive the next transaction identifier from the seed and a counter.
    ///
    /// Never random. The identifier is echoed rather than validated and the
    /// integrity attribute is what authenticates, so it only has to be unique
    /// among our outstanding transactions. Deriving it keeps the core free of a
    /// random number generator and makes a failing run replayable from its
    /// seed.
    fn next_transaction_id(&mut self) -> TransactionId {
        let counter = self.counter;
        self.counter = self.counter.wrapping_add(1);

        let mut hash = Sha1::new();
        hash.update(self.seed);
        hash.update(counter.to_be_bytes());
        let digest = hash.finalize();

        let mut tid = [0u8; 12];
        for (slot, byte) in tid.iter_mut().zip(digest.iter()) {
            *slot = *byte;
        }
        TransactionId(tid)
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
        conn.add_candidate(addr(1, 4000)).unwrap();
        conn.add_candidate(addr(2, 4000)).unwrap();

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

    /// The regression for the restore obligation. A probe is emitted at a TTL
    /// that cannot reach the peer, and every datagram after it must be back at
    /// the default; a shell that never restored would show as a path that
    /// establishes and then carries nothing over any distance.
    #[test]
    fn only_the_probe_carries_the_reduced_ttl() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000)).unwrap();

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
        conn.add_candidate(peer).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);

        assert_eq!(
            conn.process_input(&response[..len], peer).unwrap(),
            Inbound::PathEstablished(peer)
        );
        assert_eq!(conn.state(), State::Established(peer));
        assert_eq!(conn.path(), Some(peer));
    }

    #[test]
    fn checks_stop_once_a_path_is_chosen() {
        let mut conn = conn();
        let peer = addr(1, 4000);
        conn.add_candidate(peer).unwrap();
        conn.add_candidate(addr(2, 4000)).unwrap();

        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);
        conn.process_input(&response[..len], peer).unwrap();

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
        conn.add_candidate(peer).unwrap();
        let sent = drain(&mut conn, 0.0);
        let (egress, buf) = sent.last().copied().unwrap();
        let mut response = [0u8; 256];
        let len = answer(&buf, egress.len, addr(9, 5000), &mut response);
        conn.process_input(&response[..len], peer).unwrap();

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
            conn.process_input(&theirs[..len], peer).unwrap(),
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
            conn.process_input(&theirs[..len], peer),
            Err(Error::Decrypt)
        );
        assert!(
            drain(&mut conn, 1.0).is_empty(),
            "answered a check that failed authentication"
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
            conn.process_input(&response[..len], server).unwrap(),
            Inbound::Reflexive(observed)
        );
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
            conn.process_input(&response[..len], server),
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
            conn.process_input(&response[..len], addr(66, 1234)),
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
        conn.process_input(&response[..len], server).unwrap();

        assert!(
            drain(&mut conn, CHECK_CADENCE_MS + 1.0).is_empty(),
            "kept probing a server that already answered"
        );
    }

    #[test]
    fn the_window_closes_with_a_typed_failure() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000)).unwrap();

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
        conn.add_candidate(addr(1, 4000)).unwrap();
        conn.add_candidate(addr(1, 4000)).unwrap();
        assert_eq!(conn.candidate_count(), 1);

        for index in 0..MAX_CANDIDATES {
            let _ = conn.add_candidate(addr(2, 4000 + index as u16));
        }
        assert_eq!(conn.candidate_count(), MAX_CANDIDATES);
        assert_eq!(
            conn.add_candidate(addr(3, 9999)),
            Err(Error::Oversized),
            "a full table must refuse rather than evict"
        );
    }

    /// A v4-mapped candidate is the same candidate. Admitting both would spend
    /// two slots and two check budgets on one address.
    #[test]
    fn a_v4_mapped_candidate_is_not_a_second_candidate() {
        let mut conn = conn();
        conn.add_candidate(addr(1, 4000)).unwrap();
        conn.add_candidate("[::ffff:198.51.100.1]:4000".parse().unwrap())
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

        conn.add_candidate(addr(1, 4000)).unwrap();
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
