//! The relay, as a client keeps it: an allocation, a permission for every
//! address the host may be reached at, a channel for the path, and the typed
//! ways it ends. See docs/03-connectivity.md 7.
//!
//! The endpoint drives this. A datagram from the relay's address is handed
//! here before anything is classified, and everything bound for a peer is
//! wrapped on its way out, so no answer can leave outside the relay. Nothing
//! waits on the relay: a request is a deadline among the session's timers,
//! answered or sent again when it falls due, and the reader never stops for
//! it.
//!
//! ```text
//! t=0         allocate, no credentials    -> the relay's challenge
//!             allocate, with them         -> the relayed address
//!             permit the relayed address's own machine
//!             then, and only then, the relayed address may be offered
//! t=5s        not that far: the relay is unreachable
//! every 240s  each permission and each channel renewed; a permission
//!             lapses at 300s whatever crosses it
//! lifetime/2  the allocation refreshed
//! leave       a refresh of zero lifetime releases it
//! ```

use core::fmt;
use core::net::{IpAddr, SocketAddr};

use crate::conn::derive_transaction_id;
use crate::error::{Error, Result};
use crate::stun::{self, TransactionId};
use crate::turn::{
    self, Auth, Challenge, FIRST_CHANNEL, Key, MAX_NONCE, MAX_REALM, Response, STALE_NONCE,
};

/// The lifetime asked for, in seconds, and the one taken when an answer
/// grants none, or grants zero.
pub const LIFETIME_S: u32 = 600;

/// How long setup may take: the allocation, then the relay's own machine
/// permitted. A relay that has not got that far is unreachable.
pub const SETUP_DEADLINE_MS: f64 = 5_000.0;

/// A permission's life, whatever traffic crosses it.
pub const PERMISSION_MS: f64 = 300_000.0;

/// Each permission and each channel is renewed this long after it was
/// granted, well inside the permission's life.
pub const RENEW_MS: f64 = 240_000.0;

/// A channel's life.
const CHANNEL_MS: f64 = 600_000.0;

/// The first re-send of an unanswered allocation; each later one waits twice
/// as long.
pub const ALLOCATE_RESEND_MS: f64 = 500.0;

/// The re-send of any other unanswered request.
pub const RESEND_MS: f64 = 2_000.0;

/// Addresses permitted: the relay's own machine, then the host's.
pub const MAX_PERMISSIONS: usize = 8;

/// Channels: one for each address the path has followed the host to.
const MAX_CHANNELS: usize = 4;

/// Where the relay has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum State {
    /// Allocating, then permitting the relay's own machine. Nothing is
    /// offered, and nothing is sent toward a peer.
    Setup,
    /// The relayed address may be offered, and every check goes through it.
    Ready(SocketAddr),
    /// Over, and why.
    Failed(Failure),
    /// Released on a clean leave.
    Released,
}

/// Why the relay ended an attempt. Typed and final; nothing is retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Failure {
    /// The relay did not answer in time to allocate and permit.
    Unreachable,
    /// The relay refused the credentials or the allocation, or is full. The
    /// same relay refuses again.
    Refused,
    /// A renewal was refused, or went unanswered until what it renewed
    /// lapsed. The relay has already let the allocation go.
    Lost,
}

/// Whether a relay can carry anything to or from `peer`: an IPv4 address,
/// because the relayed family is IPv4, and never loopback. A deployed relay
/// destroys the allocation that sends toward loopback, so one candidate
/// there, hostile or mistaken, would end the session.
pub(crate) fn reachable(peer: SocketAddr) -> bool {
    match stun::canonical(peer).ip() {
        IpAddr::V4(ip) => !ip.is_loopback() && !ip.is_unspecified(),
        IpAddr::V6(_) => false,
    }
}

/// A request awaiting its answer.
///
/// Sent again under the same identifier. A relay that did allocate and whose
/// answer was lost recognises the identifier and answers again, where a new
/// one is refused as a second allocation on the same flow.
#[derive(Debug, Clone, Copy)]
struct Pending {
    tid: TransactionId,
    sent_ms: f64,
    wait_ms: f64,
}

/// What a thing asking one request at a time sends now: its request again
/// once that has waited long enough, or a new one once `due_ms` has come.
fn step(
    pending: &mut Option<Pending>,
    due_ms: f64,
    now_ms: f64,
    ids: &mut Ids,
    first_wait_ms: f64,
    backoff: bool,
) -> Option<TransactionId> {
    match pending {
        Some(request) if now_ms - request.sent_ms >= request.wait_ms => {
            request.sent_ms = now_ms;
            if backoff {
                request.wait_ms *= 2.0;
            }
            Some(request.tid)
        }
        Some(_) => None,
        None if now_ms >= due_ms => {
            let tid = ids.next();
            *pending = Some(Pending {
                tid,
                sent_ms: now_ms,
                wait_ms: first_wait_ms,
            });
            Some(tid)
        }
        None => None,
    }
}

/// When `step` next has something to send.
fn next_step_ms(pending: &Option<Pending>, due_ms: f64) -> f64 {
    match pending {
        Some(request) => request.sent_ms + request.wait_ms,
        None => due_ms,
    }
}

/// Transaction identifiers, derived as the punch's are and never repeated
/// within a relay's life. They must be unguessable: a refusal carries no
/// integrity, and its identifier is all that admits it.
struct Ids {
    seed: [u8; 16],
    counter: u32,
}

impl Ids {
    fn next(&mut self) -> TransactionId {
        let tid = derive_transaction_id(&self.seed, self.counter);
        self.counter = self.counter.wrapping_add(1);
        tid
    }
}

/// A permission or a channel: asked for, granted for a while, renewed.
#[derive(Debug, Clone, Copy)]
struct Lease<T> {
    target: T,
    pending: Option<Pending>,
    /// When a request is next due: at once for a new lease, then its renewal.
    due_ms: f64,
    /// When the grant lapses; `None` until it is first granted.
    expires_ms: Option<f64>,
    /// Refused, and asked for no more.
    refused: bool,
}

impl<T> Lease<T> {
    fn new(target: T) -> Self {
        Self {
            target,
            pending: None,
            due_ms: f64::NEG_INFINITY,
            expires_ms: None,
            refused: false,
        }
    }

    fn grant(&mut self, life_ms: f64, now_ms: f64) {
        self.expires_ms = Some(now_ms + life_ms);
        self.due_ms = now_ms + RENEW_MS;
    }

    fn live(&self, now_ms: f64) -> bool {
        !self.refused && self.expires_ms.is_some_and(|at| now_ms < at)
    }
}

/// What a request asks for.
#[derive(Debug, Clone, Copy)]
enum Ask {
    Allocate,
    Refresh(u32),
    Permission(IpAddr),
    Channel(u16, SocketAddr),
}

/// The relay one attempt allocates, and everything kept alive through it.
pub struct Relay<'a> {
    server: SocketAddr,
    username: &'a str,
    password: &'a str,
    ids: Ids,
    state: State,
    started_ms: f64,
    /// The local address the relay's datagrams arrive at, latched from the
    /// first and claimed by every send after it. The relay knows the
    /// allocation by the whole flow, and a source that moves is a stranger.
    local: Option<IpAddr>,
    realm: [u8; MAX_REALM],
    realm_len: usize,
    nonce: [u8; MAX_NONCE],
    nonce_len: usize,
    key: Option<Key>,
    /// The allocation request in flight, until there is an allocation.
    allocating: Option<Pending>,
    relayed: Option<SocketAddr>,
    /// When the allocation lapses without a refresh.
    expires_ms: f64,
    /// When the next refresh is due, and the refresh in flight.
    refresh_ms: f64,
    refreshing: Option<Pending>,
    permissions: [Option<Lease<IpAddr>>; MAX_PERMISSIONS],
    channels: [Option<Lease<SocketAddr>>; MAX_CHANNELS],
    /// Where media goes: the path, then wherever the host's authenticated
    /// traffic comes from.
    destination: Option<SocketAddr>,
    releasing: bool,
}

impl fmt::Debug for Relay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The credential and the key are never rendered.
        f.debug_struct("Relay")
            .field("server", &self.server)
            .field("state", &self.state)
            .field("relayed", &self.relayed)
            .field("destination", &self.destination)
            .finish_non_exhaustive()
    }
}

impl<'a> Relay<'a> {
    /// Allocate on `server` with the credential the application configured.
    /// Setup's deadline runs from now.
    ///
    /// `seed` is the transaction identifiers' and comes from the shell, where
    /// entropy lives.
    pub fn new(
        server: SocketAddr,
        username: &'a str,
        password: &'a str,
        seed: [u8; 16],
        now_ms: f64,
    ) -> Self {
        Self {
            server: stun::canonical(server),
            username,
            password,
            ids: Ids { seed, counter: 0 },
            state: State::Setup,
            started_ms: now_ms,
            local: None,
            realm: [0; MAX_REALM],
            realm_len: 0,
            nonce: [0; MAX_NONCE],
            nonce_len: 0,
            key: None,
            allocating: None,
            relayed: None,
            expires_ms: f64::INFINITY,
            refresh_ms: f64::INFINITY,
            refreshing: None,
            permissions: [None; MAX_PERMISSIONS],
            channels: [None; MAX_CHANNELS],
            destination: None,
            releasing: false,
        }
    }

    /// The relay's address. Everything from it is the relay's; everything
    /// to a peer goes to it.
    pub fn server(&self) -> SocketAddr {
        self.server
    }

    /// Where the relay has got to.
    pub fn state(&self) -> State {
        self.state
    }

    /// The relayed address, once it may be offered: allocated, and the relay's
    /// own machine permitted.
    pub fn relayed(&self) -> Option<SocketAddr> {
        match self.state {
            State::Ready(relayed) => Some(relayed),
            _ => None,
        }
    }

    /// The local address every send to the relay claims, once one has
    /// arrived from it.
    pub fn local(&self) -> Option<IpAddr> {
        self.local
    }

    /// Release the allocation with the next output, rather than hold a relay
    /// port until it expires. Everything else stops.
    pub fn release(&mut self) {
        if self.live() {
            self.releasing = true;
        }
    }

    fn live(&self) -> bool {
        matches!(self.state, State::Setup | State::Ready(_))
    }

    fn fail(&mut self, failure: Failure) {
        if self.live() {
            self.state = State::Failed(failure);
        }
    }

    /// Take a datagram that came from the relay's address. An answer is
    /// consumed here; a peer's datagram comes back unwrapped, with the peer
    /// it came from.
    pub(crate) fn unwrap<'d>(
        &mut self,
        datagram: &'d [u8],
        local: Option<IpAddr>,
        now_ms: f64,
    ) -> Result<Option<(SocketAddr, &'d [u8])>> {
        if self.local.is_none() {
            self.local = local.map(|ip| match ip {
                IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
                IpAddr::V4(_) => ip,
            });
        }
        let (peer, data) = match turn::parse(datagram)? {
            turn::Inbound::Response(response) => {
                self.answered(response, now_ms);
                return Ok(None);
            }
            turn::Inbound::Indication { peer, data } => (peer, data),
            turn::Inbound::Channel { number, data } => {
                (self.channel_peer(number).ok_or(Error::Malformed)?, data)
            }
        };
        // Nothing is relayed toward loopback, so nothing from it is taken
        // either: admitted, its source would be answered.
        if !reachable(peer) {
            return Err(Error::Malformed);
        }
        Ok(Some((peer, data)))
    }

    /// Ask the relay to admit `ip`. Once per address; a full table asks for
    /// no more, and checks toward the rest go unanswered.
    pub(crate) fn permit(&mut self, ip: IpAddr) {
        if self
            .permissions
            .iter()
            .flatten()
            .any(|lease| lease.target == ip)
        {
            return;
        }
        if let Some(slot) = self.permissions.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(Lease::new(ip));
        }
    }

    /// Whether a datagram toward `peer` can go through the relay at all.
    pub(crate) fn relayable(&self, peer: SocketAddr) -> bool {
        reachable(peer)
            && !self
                .permissions
                .iter()
                .flatten()
                .any(|lease| lease.target == peer.ip() && lease.refused)
    }

    /// Send media to `peer` from now on: the path when it is chosen, then
    /// wherever the host's authenticated traffic comes from. A channel is
    /// bound to it while any are left.
    pub(crate) fn follow(&mut self, peer: SocketAddr) {
        if self.destination == Some(peer) {
            return;
        }
        self.destination = Some(peer);
        if self
            .channels
            .iter()
            .flatten()
            .any(|lease| lease.target == peer)
        {
            return;
        }
        if let Some(slot) = self.channels.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(Lease::new(peer));
        }
    }

    /// Where media goes, once the path is chosen.
    pub(crate) fn destination(&self) -> Option<SocketAddr> {
        self.destination
    }

    /// The channel bound to `peer`, while it is.
    pub(crate) fn channel_for(&self, peer: SocketAddr, now_ms: f64) -> Option<u16> {
        let index = self.channels.iter().position(|slot| {
            slot.is_some_and(|lease| lease.target == peer && lease.live(now_ms))
        })?;
        channel_number(index)
    }

    fn channel_peer(&self, number: u16) -> Option<SocketAddr> {
        let index = usize::from(number.checked_sub(FIRST_CHANNEL)?);
        Some(self.channels.get(index)?.as_ref()?.target)
    }

    /// An identifier for an indication. It asks nothing, so nothing is kept.
    pub(crate) fn indication_id(&mut self) -> TransactionId {
        self.ids.next()
    }

    /// The next request due, into `out`. Drive until `None`.
    pub(crate) fn get_output(&mut self, now_ms: f64, out: &mut [u8]) -> Option<Result<usize>> {
        let (ask, tid) = self.next_ask(now_ms)?;
        Some(self.encode(ask, tid, out))
    }

    fn next_ask(&mut self, now_ms: f64) -> Option<(Ask, TransactionId)> {
        if self.releasing {
            self.releasing = false;
            self.state = State::Released;
            return self.relayed.map(|_| (Ask::Refresh(0), self.ids.next()));
        }
        if !self.live() {
            return None;
        }
        // Until there is an allocation nothing else can be asked for.
        if self.relayed.is_none() {
            let tid = step(
                &mut self.allocating,
                f64::NEG_INFINITY,
                now_ms,
                &mut self.ids,
                ALLOCATE_RESEND_MS,
                true,
            )?;
            return Some((Ask::Allocate, tid));
        }
        if let Some(tid) = step(
            &mut self.refreshing,
            self.refresh_ms,
            now_ms,
            &mut self.ids,
            RESEND_MS,
            false,
        ) {
            return Some((Ask::Refresh(LIFETIME_S), tid));
        }
        for lease in self.permissions.iter_mut().flatten() {
            if lease.refused {
                continue;
            }
            if let Some(tid) = step(
                &mut lease.pending,
                lease.due_ms,
                now_ms,
                &mut self.ids,
                RESEND_MS,
                false,
            ) {
                return Some((Ask::Permission(lease.target), tid));
            }
        }
        for (index, slot) in self.channels.iter_mut().enumerate() {
            let Some(lease) = slot.as_mut().filter(|lease| !lease.refused) else {
                continue;
            };
            let Some(number) = channel_number(index) else {
                continue;
            };
            if let Some(tid) = step(
                &mut lease.pending,
                lease.due_ms,
                now_ms,
                &mut self.ids,
                RESEND_MS,
                false,
            ) {
                return Some((Ask::Channel(number, lease.target), tid));
            }
        }
        None
    }

    fn encode(&self, ask: Ask, tid: TransactionId, out: &mut [u8]) -> Result<usize> {
        let auth = self.key.as_ref().map(|key| Auth {
            username: self.username,
            realm: self.realm.get(..self.realm_len).unwrap_or_default(),
            nonce: self.nonce.get(..self.nonce_len).unwrap_or_default(),
            key,
        });
        match (ask, auth.as_ref()) {
            (Ask::Allocate, auth) => turn::encode_allocate(out, tid, LIFETIME_S, auth),
            (Ask::Refresh(lifetime_s), Some(auth)) => {
                turn::encode_refresh(out, tid, lifetime_s, auth)
            }
            (Ask::Permission(ip), Some(auth)) => turn::encode_create_permission(out, tid, ip, auth),
            (Ask::Channel(number, peer), Some(auth)) => {
                turn::encode_channel_bind(out, tid, number, peer, auth)
            }
            // Nothing but the allocation is asked for before the challenge.
            (_, None) => Err(Error::Malformed),
        }
    }

    /// An answer arrived. Matched on its identifier; anything unexpected is
    /// dropped.
    fn answered(&mut self, response: Response<'_>, now_ms: f64) {
        if !self.live() {
            return;
        }
        // A success counts only under our key: its integrity is what makes
        // the relayed address and every grant the relay's word. A refusal
        // carries none, and is admitted on an identifier nobody else knows.
        if response.is_success() && !self.key.as_ref().is_some_and(|key| response.verify(key)) {
            return;
        }
        let tid = response.transaction_id();
        let answers = |pending: &Option<Pending>| pending.is_some_and(|p| p.tid == tid);
        if answers(&self.allocating) {
            self.allocated(response, now_ms);
        } else if answers(&self.refreshing) {
            self.refreshed(response, now_ms);
        } else if let Some(index) = self
            .permissions
            .iter()
            .position(|slot| slot.is_some_and(|lease| answers(&lease.pending)))
        {
            self.permitted(index, response, now_ms);
        } else if let Some(index) = self
            .channels
            .iter()
            .position(|slot| slot.is_some_and(|lease| answers(&lease.pending)))
        {
            self.bound(index, response, now_ms);
        }
    }

    fn allocated(&mut self, response: Response<'_>, now_ms: f64) {
        self.allocating = None;
        let Some(code) = response.error_code() else {
            // The relayed family is IPv4 (docs/03-connectivity.md 7.4).
            let Some(relayed) = response.relayed_address().filter(SocketAddr::is_ipv4) else {
                return self.fail(Failure::Refused);
            };
            self.relayed = Some(relayed);
            self.renewed(response.lifetime_s(), now_ms);
            // The relay's own machine first. A host there checks the relayed
            // address from its own, and a check the relay has no permission
            // for is dropped without a word.
            self.permit(relayed.ip());
            return;
        };
        // The first answer is a challenge, and so is a stale nonce at any
        // point. A challenge to credentials already sent means they are
        // wrong.
        match response.challenge() {
            Some(challenge) if code == STALE_NONCE || self.key.is_none() => self.adopt(challenge),
            _ => self.fail(Failure::Refused),
        }
    }

    fn refreshed(&mut self, response: Response<'_>, now_ms: f64) {
        self.refreshing = None;
        match (response.error_code(), response.challenge()) {
            (None, _) => self.renewed(response.lifetime_s(), now_ms),
            // Adopted and sent again at once: the refresh is still due.
            (Some(STALE_NONCE), Some(challenge)) => self.adopt(challenge),
            _ => self.fail(Failure::Lost),
        }
    }

    /// The allocation was granted for `lifetime_s`, or renewed.
    fn renewed(&mut self, lifetime_s: Option<u32>, now_ms: f64) {
        // None, or zero, is read as the lifetime asked for. Taken as zero, the
        // next refresh falls due in the past and the requests storm.
        let seconds = lifetime_s
            .filter(|&seconds| seconds > 0)
            .unwrap_or(LIFETIME_S);
        let lifetime_ms = f64::from(seconds) * 1000.0;
        self.expires_ms = now_ms + lifetime_ms;
        self.refresh_ms = now_ms + lifetime_ms / 2.0;
    }

    fn permitted(&mut self, index: usize, response: Response<'_>, now_ms: f64) {
        let stale = response.error_code() == Some(STALE_NONCE);
        if let (true, Some(challenge)) = (stale, response.challenge()) {
            self.adopt(challenge);
        }
        let destination = self.destination.map(|peer| peer.ip());
        let Some(lease) = self.permissions.get_mut(index).and_then(Option::as_mut) else {
            return;
        };
        lease.pending = None;
        let lost = match response.error_code() {
            None => {
                lease.grant(PERMISSION_MS, now_ms);
                false
            }
            // Sent again at once: it is still due.
            Some(STALE_NONCE) if response.challenge().is_some() => false,
            // A relay may refuse one address and admit the rest, so this one
            // is asked for no more and the others carry on. Only the path's
            // own address, refused on renewal, loses the relay.
            Some(_) => {
                let renewal = lease.expires_ms.is_some();
                lease.refused = true;
                renewal && destination == Some(lease.target)
            }
        };
        if lost {
            self.fail(Failure::Lost);
        }
        self.settle();
    }

    fn bound(&mut self, index: usize, response: Response<'_>, now_ms: f64) {
        let stale = response.error_code() == Some(STALE_NONCE);
        if let (true, Some(challenge)) = (stale, response.challenge()) {
            self.adopt(challenge);
        }
        let Some(lease) = self.channels.get_mut(index).and_then(Option::as_mut) else {
            return;
        };
        lease.pending = None;
        match response.error_code() {
            None => {
                lease.grant(CHANNEL_MS, now_ms);
                // A binding renews its address's permission too.
                let ip = lease.target.ip();
                if let Some(permission) = self
                    .permissions
                    .iter_mut()
                    .flatten()
                    .find(|permission| permission.target == ip && !permission.refused)
                {
                    permission.expires_ms = Some(now_ms + PERMISSION_MS);
                }
            }
            Some(STALE_NONCE) if response.challenge().is_some() => {}
            // Media stays on indications; nothing is lost but four bytes a
            // datagram.
            Some(_) => lease.refused = true,
        }
    }

    /// Setup is over once the relayed address exists and its own machine's
    /// permission has an answer, granted or refused: a relay elsewhere may
    /// refuse its own address, and then no host is there to need it.
    fn settle(&mut self) {
        let (State::Setup, Some(relayed)) = (self.state, self.relayed) else {
            return;
        };
        if self.permissions.iter().flatten().any(|lease| {
            lease.target == relayed.ip() && (lease.refused || lease.expires_ms.is_some())
        }) {
            self.state = State::Ready(relayed);
        }
    }

    /// Take a challenge's realm and nonce. A new realm is a new key.
    fn adopt(&mut self, challenge: Challenge<'_>) {
        let (Some(realm), Some(nonce)) = (
            self.realm.get_mut(..challenge.realm.len()),
            self.nonce.get_mut(..challenge.nonce.len()),
        ) else {
            return;
        };
        let same_realm = self.realm_len == challenge.realm.len() && *realm == *challenge.realm;
        if !same_realm || self.key.is_none() {
            realm.copy_from_slice(challenge.realm);
            self.realm_len = challenge.realm.len();
            self.key = Some(Key::long_term(
                self.username,
                challenge.realm,
                self.password,
            ));
        }
        nonce.copy_from_slice(challenge.nonce);
        self.nonce_len = challenge.nonce.len();
    }

    /// Housekeeping: setup's deadline, and anything renewed too late.
    pub(crate) fn poll(&mut self, now_ms: f64) {
        match self.state {
            State::Setup if now_ms - self.started_ms >= SETUP_DEADLINE_MS => {
                self.fail(Failure::Unreachable);
            }
            State::Ready(_) if now_ms >= self.expires_ms || self.path_lapsed(now_ms) => {
                self.fail(Failure::Lost);
            }
            _ => {}
        }
    }

    /// Whether the permission for the path's address has lapsed. The relay
    /// then drops both directions without a word.
    fn path_lapsed(&self, now_ms: f64) -> bool {
        let Some(peer) = self.destination else {
            return false;
        };
        self.permissions.iter().flatten().any(|lease| {
            lease.target == peer.ip() && lease.expires_ms.is_some_and(|at| now_ms >= at)
        })
    }

    /// Milliseconds until the relay next needs attention.
    pub(crate) fn next_timer_ms(&self, now_ms: f64) -> f64 {
        if self.releasing {
            return 0.0;
        }
        let mut soonest = match self.state {
            State::Setup => self.started_ms + SETUP_DEADLINE_MS,
            State::Ready(_) => self.expires_ms,
            State::Failed(_) | State::Released => return f64::INFINITY,
        };
        if self.relayed.is_none() {
            soonest = soonest.min(next_step_ms(&self.allocating, f64::NEG_INFINITY));
        } else {
            soonest = soonest.min(next_step_ms(&self.refreshing, self.refresh_ms));
            for lease in self.permissions.iter().flatten() {
                if !lease.refused {
                    soonest = soonest.min(next_step_ms(&lease.pending, lease.due_ms));
                }
                if self
                    .destination
                    .is_some_and(|peer| peer.ip() == lease.target)
                {
                    soonest = soonest.min(lease.expires_ms.unwrap_or(f64::INFINITY));
                }
            }
            for lease in self
                .channels
                .iter()
                .flatten()
                .filter(|lease| !lease.refused)
            {
                soonest = soonest.min(next_step_ms(&lease.pending, lease.due_ms));
            }
        }
        (soonest - now_ms).max(0.0)
    }
}

fn channel_number(index: usize) -> Option<u16> {
    FIRST_CHANNEL.checked_add(u16::try_from(index).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::turn::testing::{
        allocated, challenge, channel_of, granted, kind_of, lifetime_of, nonce_of, peer_of,
        refusal, signed, tid_of,
    };
    use core::net::Ipv4Addr;
    use std::vec::Vec;

    const USER: &str = "user";
    const PASS: &str = "password";
    const REALM: &[u8] = b"relay.example";
    const NONCE: &[u8] = b"5d1b0a4f3c2e7a90";

    const ALLOCATE: u16 = 0x0003;
    const REFRESH: u16 = 0x0004;
    const PERMISSION: u16 = 0x0008;
    const CHANNEL: u16 = 0x0009;

    fn server() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)), 3478)
    }

    /// The relayed address: the relay's own machine, as a deployed relay
    /// with no external address hands out.
    fn relayed_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 50_048)
    }

    fn host(last: u8) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, last)), 22_974)
    }

    fn key() -> Key {
        Key::long_term(USER, REALM, PASS)
    }

    fn relay() -> Relay<'static> {
        Relay::new(server(), USER, PASS, [0x3C; 16], 0.0)
    }

    /// Everything the relay wants to send at `now`.
    fn drain(relay: &mut Relay<'_>, now: f64) -> Vec<Vec<u8>> {
        let mut sent = Vec::new();
        let mut out = [0u8; 512];
        while let Some(result) = relay.get_output(now, &mut out) {
            sent.push(out[..result.unwrap()].to_vec());
        }
        sent
    }

    /// The one request the relay wants to send at `now`.
    fn one(relay: &mut Relay<'_>, now: f64) -> Vec<u8> {
        let mut sent = drain(relay, now);
        assert_eq!(
            sent.len(),
            1,
            "expected one request at {now} ms, got {}",
            sent.len()
        );
        sent.remove(0)
    }

    /// The one request of `kind` among everything sent at `now`.
    fn pick(relay: &mut Relay<'_>, now: f64, kind: u16) -> Vec<u8> {
        let mut sent: Vec<_> = drain(relay, now)
            .into_iter()
            .filter(|request| kind_of(request) == kind)
            .collect();
        assert_eq!(
            sent.len(),
            1,
            "expected one {kind:#06x} at {now} ms, got {}",
            sent.len()
        );
        sent.remove(0)
    }

    /// Whether nothing of `kind` is sent at `now`.
    fn none_of(relay: &mut Relay<'_>, now: f64, kind: u16) -> bool {
        drain(relay, now)
            .iter()
            .all(|request| kind_of(request) != kind)
    }

    fn answer(relay: &mut Relay<'_>, bytes: &[u8], now: f64) {
        assert_eq!(relay.unwrap(bytes, None, now).unwrap(), None);
    }

    /// Challenged, then allocated for `lifetime_s`; the relay's own machine
    /// is asked for next and its request returned unanswered.
    fn allocate(relay: &mut Relay<'_>, lifetime_s: Option<u32>, now: f64) -> Vec<u8> {
        let first = one(relay, now);
        answer(relay, &challenge(&first, 401, REALM, NONCE), now);
        let second = one(relay, now);
        answer(
            relay,
            &allocated(&second, relayed_addr(), lifetime_s, &key()),
            now,
        );
        one(relay, now)
    }

    /// Allocated for 600 s and the relay's own machine permitted: ready.
    fn ready(relay: &mut Relay<'_>, now: f64) {
        let permission = allocate(relay, Some(600), now);
        answer(relay, &granted(&permission, None, &key()), now);
        assert_eq!(relay.state(), State::Ready(relayed_addr()));
    }

    /// The order the whole design rests on. A relayed address offered before
    /// its own machine is permitted has the host's first checks dropped at
    /// the relay, and the host's punch then finishes after ours.
    #[test]
    fn the_box_address_is_permitted_before_the_candidate_is_offered() {
        let mut relay = relay();
        let first = one(&mut relay, 0.0);
        assert_eq!(kind_of(&first), ALLOCATE);
        assert!(!signed(&first), "the first request carried credentials");
        assert_eq!(relay.relayed(), None);

        answer(&mut relay, &challenge(&first, 401, REALM, NONCE), 20.0);
        let second = one(&mut relay, 20.0);
        assert_eq!(kind_of(&second), ALLOCATE);
        assert!(signed(&second));
        assert_eq!(nonce_of(&second).as_deref(), Some(NONCE));

        answer(
            &mut relay,
            &allocated(&second, relayed_addr(), Some(600), &key()),
            40.0,
        );
        assert_eq!(
            relay.relayed(),
            None,
            "offered before its machine was permitted"
        );
        let permission = one(&mut relay, 40.0);
        assert_eq!(kind_of(&permission), PERMISSION);
        assert_eq!(
            peer_of(&permission).map(|peer| peer.ip()),
            Some(relayed_addr().ip())
        );

        answer(&mut relay, &granted(&permission, None, &key()), 60.0);
        assert_eq!(relay.relayed(), Some(relayed_addr()));
    }

    /// A relay somewhere other than the host's machine may refuse its own
    /// address. No host is there to need it, so setup is over all the same.
    #[test]
    fn a_relay_that_refuses_its_own_address_is_still_ready() {
        let mut relay = relay();
        let permission = allocate(&mut relay, Some(600), 0.0);
        answer(&mut relay, &refusal(&permission, 403), 10.0);
        assert_eq!(relay.relayed(), Some(relayed_addr()));
    }

    /// A permission lasts 300 seconds and traffic does not extend it, so it is
    /// asked for again well before. Left to lapse, the relay drops both
    /// directions and the session freezes at five minutes.
    #[test]
    fn a_permission_is_reissued_before_three_hundred_seconds() {
        let mut relay = relay();
        let permission = allocate(&mut relay, Some(3600), 0.0);
        answer(&mut relay, &granted(&permission, None, &key()), 0.0);

        // Three renewals cross 300 seconds three times; each lands before the
        // grant it renews has lapsed.
        let mut granted_at = 0.0;
        for round in 1..=3 {
            let due = f64::from(round) * RENEW_MS;
            assert!(drain(&mut relay, due - 1.0).is_empty(), "renewed early");
            let renewal = one(&mut relay, due);
            assert_eq!(kind_of(&renewal), PERMISSION);
            assert!(due < granted_at + PERMISSION_MS, "renewed after it lapsed");
            answer(&mut relay, &granted(&renewal, None, &key()), due);
            granted_at = due;
            relay.poll(due);
        }
        assert_eq!(relay.state(), State::Ready(relayed_addr()));
    }

    #[test]
    fn the_allocation_is_refreshed_at_half_its_lifetime() {
        for lifetime in [600u32, 100] {
            let mut relay = relay();
            let permission = allocate(&mut relay, Some(lifetime), 0.0);
            answer(&mut relay, &granted(&permission, None, &key()), 0.0);

            let half = f64::from(lifetime) * 500.0;
            assert!(none_of(&mut relay, half - 1.0, REFRESH), "refreshed early");
            let refresh = pick(&mut relay, half, REFRESH);
            assert_eq!(lifetime_of(&refresh), Some(LIFETIME_S));
            answer(&mut relay, &granted(&refresh, Some(lifetime), &key()), half);
            assert!(none_of(&mut relay, 2.0 * half - 1.0, REFRESH));
            let _ = pick(&mut relay, 2.0 * half, REFRESH);
        }
    }

    /// An answer with no lifetime, or a zero one, grants what was asked for.
    /// Taken literally, the next refresh falls due in the past, and every pass
    /// sends another.
    #[test]
    fn a_zero_lifetime_is_read_as_the_request() {
        for granted_s in [None, Some(0)] {
            let mut relay = relay();
            let permission = allocate(&mut relay, granted_s, 0.0);
            answer(&mut relay, &granted(&permission, None, &key()), 0.0);
            assert!(
                drain(&mut relay, 1_000.0).is_empty(),
                "a lifetime of {granted_s:?} set off a refresh at once"
            );
            let half = f64::from(LIFETIME_S) * 500.0;
            let _ = pick(&mut relay, half, REFRESH);
        }
    }

    /// Permissions are independent. A relay may refuse one address and admit
    /// another, and the refusal sinks nothing but itself; the refused address
    /// is not asked for again.
    #[test]
    fn a_refused_permission_does_not_sink_the_others() {
        let mut relay = relay();
        ready(&mut relay, 0.0);
        relay.permit(host(30).ip());
        relay.permit(host(40).ip());
        let requests = drain(&mut relay, 10.0);
        assert_eq!(requests.len(), 2, "one request an address");
        for request in &requests {
            assert_eq!(kind_of(request), PERMISSION);
        }

        answer(&mut relay, &refusal(&requests[0], 403), 20.0);
        answer(&mut relay, &granted(&requests[1], None, &key()), 20.0);
        assert_eq!(relay.state(), State::Ready(relayed_addr()));
        assert!(!relay.relayable(host(30)));
        assert!(relay.relayable(host(40)));
        let later: Vec<_> = drain(&mut relay, RENEW_MS + 20.0)
            .into_iter()
            .filter_map(|request| peer_of(&request))
            .collect();
        assert!(
            later.iter().all(|peer| peer.ip() != host(30).ip()),
            "asked again"
        );
        assert!(
            later.iter().any(|peer| peer.ip() == host(40).ip()),
            "not renewed"
        );
    }

    /// A permission with no answer is asked for again, under the same
    /// identifier, every two seconds. Recorded as granted before it was, it
    /// would never be asked for at all.
    #[test]
    fn an_unanswered_permission_is_resent() {
        let mut relay = relay();
        ready(&mut relay, 0.0);
        relay.permit(host(30).ip());
        let first = one(&mut relay, 100.0);
        assert!(drain(&mut relay, 100.0 + RESEND_MS - 1.0).is_empty());
        let again = one(&mut relay, 100.0 + RESEND_MS);
        assert_eq!(tid_of(&again), tid_of(&first));
        assert_eq!(peer_of(&again), peer_of(&first));
        assert_eq!(relay.state(), State::Ready(relayed_addr()));
    }

    /// An allocation whose answer was lost is asked for again under the same
    /// identifier. A new one is a second allocation on the same flow, which a
    /// relay refuses.
    #[test]
    fn an_unanswered_allocation_is_sent_again_as_itself() {
        let mut relay = relay();
        let first = one(&mut relay, 0.0);
        answer(&mut relay, &challenge(&first, 401, REALM, NONCE), 0.0);
        let second = one(&mut relay, 0.0);
        let mut at = 0.0;
        let mut wait = ALLOCATE_RESEND_MS;
        for _ in 0..3 {
            assert!(drain(&mut relay, at + wait - 1.0).is_empty());
            at += wait;
            assert_eq!(tid_of(&one(&mut relay, at)), tid_of(&second));
            wait *= 2.0;
        }
    }

    /// A relay rotates its nonce and answers the next request with a stale
    /// nonce error. The new one is adopted and the request sent again at once,
    /// under a new identifier, and the relay carries on.
    #[test]
    fn a_stale_nonce_is_adopted_and_the_request_sent_again() {
        let rotated: &[u8] = b"a0b1c2d3e4f5a6b7";
        let mut relay = relay();
        ready(&mut relay, 0.0);
        let half = f64::from(LIFETIME_S) * 500.0;
        let refresh = pick(&mut relay, half, REFRESH);
        answer(&mut relay, &challenge(&refresh, 438, REALM, rotated), half);
        let again = pick(&mut relay, half, REFRESH);
        assert_ne!(tid_of(&again), tid_of(&refresh));
        assert_eq!(nonce_of(&again).as_deref(), Some(rotated));
        answer(&mut relay, &granted(&again, Some(600), &key()), half);
        assert_eq!(relay.state(), State::Ready(relayed_addr()));
    }

    /// A success that does not authenticate under our key is not the relay's
    /// word, and is dropped as though it never came.
    #[test]
    fn an_answer_that_does_not_authenticate_is_not_taken() {
        let mut relay = relay();
        let first = one(&mut relay, 0.0);
        answer(&mut relay, &challenge(&first, 401, REALM, NONCE), 0.0);
        let second = one(&mut relay, 0.0);
        let forged = allocated(
            &second,
            host(66),
            Some(600),
            &Key::long_term(USER, REALM, "guess"),
        );
        answer(&mut relay, &forged, 10.0);
        assert_eq!(relay.state(), State::Setup);
        assert!(relay.relayed.is_none(), "a forged allocation was taken");
    }

    #[test]
    fn a_relay_that_never_answers_is_unreachable() {
        let mut relay = relay();
        let mut now = 0.0;
        while now < SETUP_DEADLINE_MS {
            let _ = drain(&mut relay, now);
            relay.poll(now);
            assert_eq!(relay.state(), State::Setup);
            now += 100.0;
        }
        relay.poll(SETUP_DEADLINE_MS);
        assert_eq!(relay.state(), State::Failed(Failure::Unreachable));
        assert!(
            drain(&mut relay, SETUP_DEADLINE_MS).is_empty(),
            "a failed relay still asks"
        );
    }

    /// A challenge to credentials already sent means they are wrong; a relay
    /// that is full, or refuses outright, refuses again. Neither is retried.
    #[test]
    fn a_refusal_of_the_allocation_is_refused() {
        for code in [401u16, 403, 437, 486, 508] {
            let mut relay = relay();
            let first = one(&mut relay, 0.0);
            answer(&mut relay, &challenge(&first, 401, REALM, NONCE), 0.0);
            let second = one(&mut relay, 0.0);
            let refused = if code == 401 {
                challenge(&second, 401, REALM, NONCE)
            } else {
                refusal(&second, code)
            };
            answer(&mut relay, &refused, 10.0);
            assert_eq!(
                relay.state(),
                State::Failed(Failure::Refused),
                "code {code}"
            );
        }
    }

    /// A refresh refused mid-session means the relay has let the allocation
    /// go, and one never answered lapses with it.
    #[test]
    fn a_refused_or_unanswered_refresh_loses_the_relay() {
        let half = f64::from(LIFETIME_S) * 500.0;
        let mut relay = relay();
        ready(&mut relay, 0.0);
        let refresh = pick(&mut relay, half, REFRESH);
        answer(&mut relay, &refusal(&refresh, 437), half);
        assert_eq!(relay.state(), State::Failed(Failure::Lost));

        let mut relay = self::relay();
        ready(&mut relay, 0.0);
        let mut now = half;
        while now < 2.0 * half {
            let _ = drain(&mut relay, now);
            relay.poll(now);
            assert!(matches!(relay.state(), State::Ready(_)), "lost at {now}");
            now += RESEND_MS;
        }
        relay.poll(2.0 * half);
        assert_eq!(relay.state(), State::Failed(Failure::Lost));
    }

    /// The path's own permission refused on renewal means the relay will
    /// drop both directions; so does it lapsing unanswered.
    #[test]
    fn the_paths_permission_refused_or_lapsed_loses_the_relay() {
        let mut relay = relay();
        ready(&mut relay, 0.0);
        relay.permit(host(30).ip());
        let first = one(&mut relay, 0.0);
        answer(&mut relay, &granted(&first, None, &key()), 0.0);
        relay.follow(host(30));
        let bind = one(&mut relay, 0.0);
        assert_eq!(kind_of(&bind), CHANNEL);
        answer(&mut relay, &granted(&bind, None, &key()), 0.0);

        let renewals = drain(&mut relay, RENEW_MS);
        let path = renewals
            .iter()
            .find(|request| {
                kind_of(request) == PERMISSION
                    && peer_of(request).map(|p| p.ip()) == Some(host(30).ip())
            })
            .expect("the path's permission was not renewed");
        answer(&mut relay, &refusal(path, 403), RENEW_MS);
        assert_eq!(relay.state(), State::Failed(Failure::Lost));

        let mut relay = self::relay();
        ready(&mut relay, 0.0);
        relay.permit(host(30).ip());
        let first = one(&mut relay, 0.0);
        answer(&mut relay, &granted(&first, None, &key()), 0.0);
        relay.follow(host(30));
        let _ = drain(&mut relay, 0.0);
        relay.poll(PERMISSION_MS - 1.0);
        assert!(matches!(relay.state(), State::Ready(_)));
        relay.poll(PERMISSION_MS);
        assert_eq!(relay.state(), State::Failed(Failure::Lost));
    }

    /// Media goes on a channel once one is bound to where it goes, and a new
    /// destination gets a channel of its own; channel data comes back named
    /// by the number the binding gave it.
    #[test]
    fn a_channel_is_bound_to_where_media_goes() {
        let mut relay = relay();
        ready(&mut relay, 0.0);
        relay.follow(host(30));
        assert_eq!(relay.channel_for(host(30), 0.0), None);
        let bind = one(&mut relay, 0.0);
        assert_eq!(channel_of(&bind), Some(FIRST_CHANNEL));
        assert_eq!(peer_of(&bind), Some(host(30)));
        answer(&mut relay, &granted(&bind, None, &key()), 10.0);
        assert_eq!(relay.channel_for(host(30), 10.0), Some(FIRST_CHANNEL));
        assert_eq!(relay.channel_peer(FIRST_CHANNEL), Some(host(30)));

        relay.follow(host(40));
        assert_eq!(relay.destination(), Some(host(40)));
        let bind = one(&mut relay, 20.0);
        assert_eq!(channel_of(&bind), Some(FIRST_CHANNEL + 1));
        assert_eq!(
            relay.channel_for(host(40), 20.0),
            None,
            "used before it was bound"
        );

        // Lapsed unrenewed, a channel is no longer used.
        assert_eq!(relay.channel_for(host(30), 10.0 + CHANNEL_MS), None);
    }

    /// On a clean leave the allocation is released with a refresh of zero
    /// lifetime, rather than held until it expires, and nothing follows it.
    #[test]
    fn a_clean_leave_releases_the_allocation() {
        let mut relay = relay();
        ready(&mut relay, 0.0);
        relay.release();
        let release = one(&mut relay, 1_000.0);
        assert_eq!(kind_of(&release), REFRESH);
        assert_eq!(lifetime_of(&release), Some(0));
        assert!(signed(&release));
        assert_eq!(relay.state(), State::Released);
        assert!(drain(&mut relay, RENEW_MS).is_empty());
    }

    /// Nothing is ever relayed toward loopback, and nothing from it is taken:
    /// a deployed relay destroys the allocation that sends there.
    #[test]
    fn loopback_is_never_reachable() {
        for peer in [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50_000),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 8, 9, 10)), 9),
            "[::1]:50000".parse().unwrap(),
            "[::ffff:127.0.0.1]:50000".parse().unwrap(),
        ] {
            assert!(!reachable(peer), "{peer}");
        }
        assert!(reachable(host(30)));
    }
}
