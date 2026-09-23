//! A relay server on the simulated network: allocations, permissions,
//! channels, and the behaviours of a deployed one.
//!
//! **Written from the standard's description of the wire, not from the
//! client's codec.** A client and a server written from one misreading agree
//! with each other perfectly; the standard's own test vector and a real relay
//! are what break that tie, and this is the third party that makes a relayed
//! run in the simulator mean anything. Nothing here is shared with the core.
//!
//! Beyond the standard, what a deployed relay was measured doing:
//!
//! - a permission lasts 300 seconds, whatever traffic crosses it;
//! - the nonce rotates, and a request with the old one is refused with a
//!   stale-nonce error that carries the new one;
//! - a relayed send toward loopback destroys the allocation that sent it;
//! - it delivers channel data unpadded, and a relayed datagram with its data
//!   ahead of the peer's address;
//! - a refusal carries no integrity.

use std::net::{IpAddr, SocketAddr};

use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use sha1::Sha1;

use crate::{Arrival, HostId, NatId, Sim};

const COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

// Message types: method and class, interleaved as the standard lays them out.
const ALLOCATE: u16 = 0x0003;
const REFRESH: u16 = 0x0004;
const CREATE_PERMISSION: u16 = 0x0008;
const CHANNEL_BIND: u16 = 0x0009;
const SEND_INDICATION: u16 = 0x0016;
const DATA_INDICATION: u16 = 0x0017;
const SUCCESS: u16 = 0x0100;
const ERROR: u16 = 0x0110;

const USERNAME: u16 = 0x0006;
const MESSAGE_INTEGRITY: u16 = 0x0008;
const ERROR_CODE: u16 = 0x0009;
const CHANNEL_NUMBER: u16 = 0x000C;
const LIFETIME: u16 = 0x000D;
const XOR_PEER_ADDRESS: u16 = 0x0012;
const DATA: u16 = 0x0013;
const REALM: u16 = 0x0014;
const NONCE: u16 = 0x0015;
const XOR_RELAYED_ADDRESS: u16 = 0x0016;
const REQUESTED_TRANSPORT: u16 = 0x0019;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;

const PERMISSION_MS: f64 = 300_000.0;
const CHANNEL_MS: f64 = 600_000.0;
const DEFAULT_LIFETIME_S: u32 = 600;
const MAX_LIFETIME_S: u32 = 3_600;
const FIRST_RELAYED_PORT: u16 = 49_152;
const TTL: u8 = 64;

/// What the relay did, for a test to assert the mechanism rather than the
/// outcome alone.
#[derive(Debug, Default, Clone)]
pub struct Counts {
    pub allocations: u32,
    pub refreshes: u32,
    pub releases: u32,
    pub permissions: u32,
    pub binds: u32,
    /// Challenges to a request without credentials, or with wrong ones.
    pub challenges: u32,
    pub stale_nonces: u32,
    /// Allocations destroyed by a relayed send toward loopback.
    pub destroyed: u32,
    /// Datagrams dropped for want of a permission, either way.
    pub unpermitted: u32,
    /// Peers' datagrams the client sent, by framing, and the largest of each.
    pub indications_in: u32,
    pub channel_in: u32,
    pub largest_indication_in: usize,
    pub largest_channel_in: usize,
    /// Peers' datagrams delivered to the client, likewise.
    pub indications_out: u32,
    pub channel_out: u32,
    pub largest_indication_out: usize,
    pub largest_channel_out: usize,
}

#[derive(Debug)]
struct Allocation {
    /// The client's address as the relay sees it: with the relay's own, the
    /// flow that names the allocation.
    client: SocketAddr,
    /// The allocation request's identifier, so a re-sent one is recognised.
    tid: [u8; 12],
    relayed: SocketAddr,
    host: HostId,
    lifetime_s: u32,
    expires_ms: f64,
    alive: bool,
    permissions: Vec<(IpAddr, f64)>,
    channels: Vec<(u16, SocketAddr, f64)>,
}

impl Allocation {
    fn permitted(&self, ip: IpAddr, now_ms: f64) -> bool {
        self.permissions
            .iter()
            .any(|(permitted, until)| *permitted == ip && now_ms < *until)
    }

    fn permit(&mut self, ip: IpAddr, now_ms: f64) {
        self.permissions.retain(|(permitted, _)| *permitted != ip);
        self.permissions.push((ip, now_ms + PERMISSION_MS));
    }

    fn channel_to(&self, peer: SocketAddr, now_ms: f64) -> Option<u16> {
        self.channels
            .iter()
            .find(|(_, bound, until)| *bound == peer && now_ms < *until)
            .map(|(number, _, _)| *number)
    }

    fn channel_peer(&self, number: u16, now_ms: f64) -> Option<SocketAddr> {
        self.channels
            .iter()
            .find(|(bound, _, until)| *bound == number && now_ms < *until)
            .map(|(_, peer, _)| *peer)
    }
}

/// A message, its attributes found in order, and where its integrity is.
struct Message<'a> {
    bytes: &'a [u8],
    kind: u16,
    tid: [u8; 12],
    attributes: Vec<(u16, &'a [u8])>,
    integrity_at: Option<usize>,
}

impl<'a> Message<'a> {
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < 20 || bytes[0] >> 6 != 0 || bytes[4..8] != COOKIE {
            return None;
        }
        let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        if length != bytes.len() - 20 {
            return None;
        }
        let mut attributes = Vec::new();
        let mut integrity_at = None;
        let mut at = 20;
        while at < bytes.len() {
            let head = bytes.get(at..at + 4)?;
            let kind = u16::from_be_bytes([head[0], head[1]]);
            let len = usize::from(u16::from_be_bytes([head[2], head[3]]));
            let value = bytes.get(at + 4..at + 4 + len)?;
            // What follows the integrity is outside what it covers.
            if integrity_at.is_none() {
                if kind == MESSAGE_INTEGRITY {
                    integrity_at = Some(at);
                } else {
                    attributes.push((kind, value));
                }
            }
            at += 4 + len.div_ceil(4) * 4;
        }
        if at != bytes.len() {
            return None;
        }
        Some(Self {
            bytes,
            kind: u16::from_be_bytes([bytes[0], bytes[1]]),
            tid: bytes[8..20].try_into().ok()?,
            attributes,
            integrity_at,
        })
    }

    fn get(&self, kind: u16) -> Option<&'a [u8]> {
        self.attributes
            .iter()
            .find(|(found, _)| *found == kind)
            .map(|(_, value)| *value)
    }
}

/// An address attribute's value: the port and address masked with the
/// cookie, and an IPv6 address with the transaction identifier after it.
fn xor_encode(addr: SocketAddr, tid: &[u8; 12]) -> Vec<u8> {
    let mask: Vec<u8> = COOKIE.iter().chain(tid.iter()).copied().collect();
    let port = addr.port() ^ 0x2112;
    let (family, octets) = match addr.ip() {
        IpAddr::V4(ip) => (1, ip.octets().to_vec()),
        IpAddr::V6(ip) => (2, ip.octets().to_vec()),
    };
    let mut value = vec![0, family];
    value.extend_from_slice(&port.to_be_bytes());
    value.extend(octets.iter().zip(&mask).map(|(byte, mask)| byte ^ mask));
    value
}

fn xor_decode(value: &[u8], tid: &[u8; 12]) -> Option<SocketAddr> {
    let mask: Vec<u8> = COOKIE.iter().chain(tid.iter()).copied().collect();
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]) ^ 0x2112;
    let unmask = |len: usize| -> Option<Vec<u8>> {
        Some(
            value
                .get(4..4 + len)?
                .iter()
                .zip(&mask)
                .map(|(byte, mask)| byte ^ mask)
                .collect(),
        )
    };
    let ip = match value.get(1)? {
        1 => IpAddr::from(<[u8; 4]>::try_from(unmask(4)?).ok()?),
        2 => IpAddr::from(<[u8; 16]>::try_from(unmask(16)?).ok()?),
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

fn be16(value: usize) -> [u8; 2] {
    u16::try_from(value)
        .expect("a relay message fits a datagram")
        .to_be_bytes()
}

fn attribute(out: &mut Vec<u8>, kind: u16, value: &[u8]) {
    out.extend_from_slice(&kind.to_be_bytes());
    out.extend_from_slice(&be16(value.len()));
    out.extend_from_slice(value);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

fn set_length(out: &mut [u8], length: usize) {
    out[2..4].copy_from_slice(&be16(length));
}

/// An error code's value: the hundreds, then the rest, and no reason phrase.
fn error_code(code: u16) -> [u8; 4] {
    let [_, hundreds] = (code / 100).to_be_bytes();
    let [_, rest] = (code % 100).to_be_bytes();
    [0, 0, hundreds, rest]
}

fn header(kind: u16, tid: &[u8; 12]) -> Vec<u8> {
    let mut out = kind.to_be_bytes().to_vec();
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&COOKIE);
    out.extend_from_slice(tid);
    out
}

/// A relay on the simulated network.
#[derive(Debug)]
pub struct Relay {
    listener: HostId,
    relay_ip: IpAddr,
    chain: Vec<NatId>,
    username: String,
    realm: Vec<u8>,
    /// The long-term key: a digest of the username, the realm and the password.
    key: [u8; 16],
    nonce_life_ms: f64,
    next_port: u16,
    next_tid: u32,
    allocations: Vec<Allocation>,
    refused: Vec<IpAddr>,
    binds_channels: bool,
    pub counts: Counts,
}

impl Relay {
    /// A relay listening at `listen` behind `chain`, allocating relayed
    /// addresses on `relay_ip` behind the same chain. On a public server the
    /// chain is empty and the two are the same address; on the host's own
    /// machine `listen` is reached through a forwarded port and `relay_ip` is
    /// the machine's own address, which nothing outside can reach.
    pub fn new(
        sim: &mut Sim,
        listen: SocketAddr,
        relay_ip: IpAddr,
        chain: &[NatId],
        username: &str,
        password: &str,
    ) -> Self {
        let realm = b"relay.example".to_vec();
        let mut digest = Md5::new();
        digest.update(username.as_bytes());
        digest.update(b":");
        digest.update(&realm);
        digest.update(b":");
        digest.update(password.as_bytes());
        Self {
            listener: sim.add_host(listen, chain),
            relay_ip,
            chain: chain.to_vec(),
            username: username.to_string(),
            realm,
            key: digest.finalize().into(),
            nonce_life_ms: f64::INFINITY,
            next_port: FIRST_RELAYED_PORT,
            next_tid: 0,
            allocations: Vec::new(),
            refused: Vec::new(),
            binds_channels: true,
            counts: Counts::default(),
        }
    }

    /// Rotate the nonce every `life_ms`.
    pub fn rotating_nonce_every(mut self, life_ms: f64) -> Self {
        self.nonce_life_ms = life_ms;
        self
    }

    /// Refuse a permission for `ip`.
    pub fn refusing(mut self, ip: IpAddr) -> Self {
        self.refused.push(ip);
        self
    }

    /// Refuse every channel, so everything relayed goes as indications.
    pub fn refusing_channels(mut self) -> Self {
        self.binds_channels = false;
        self
    }

    /// Whether `host` is one of the relay's own addresses.
    pub fn owns(&self, host: HostId) -> bool {
        host == self.listener || self.allocations.iter().any(|a| a.host == host)
    }

    /// Forget every allocation, as a relay that restarted has.
    pub fn forget_allocations(&mut self) {
        for allocation in &mut self.allocations {
            allocation.alive = false;
        }
    }

    /// Take a datagram that arrived at one of the relay's addresses.
    pub fn receive(&mut self, sim: &mut Sim, arrival: &Arrival) {
        if arrival.host == self.listener {
            self.client_sent(sim, arrival.from, &arrival.bytes);
        } else {
            self.peer_sent(sim, arrival);
        }
    }

    /// The nonce in force at `now_ms`: one per life, numbered from zero.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "simulated time is small and never negative"
    )]
    fn nonce(&self, now_ms: f64) -> Vec<u8> {
        let epoch = if self.nonce_life_ms.is_finite() {
            (now_ms / self.nonce_life_ms).floor() as u64
        } else {
            0
        };
        format!("{epoch:016x}").into_bytes()
    }

    fn live(&mut self, client: SocketAddr, now_ms: f64) -> Option<&mut Allocation> {
        self.allocations
            .iter_mut()
            .find(|a| a.client == client && a.alive && now_ms < a.expires_ms)
    }

    fn client_sent(&mut self, sim: &mut Sim, client: SocketAddr, bytes: &[u8]) {
        let now = sim.now_ms();
        if bytes.first().is_some_and(|byte| byte >> 6 == 0b01) {
            return self.channel_from_client(sim, client, bytes, now);
        }
        let Some(message) = Message::parse(bytes) else {
            return;
        };
        match message.kind {
            SEND_INDICATION => self.send_from_client(sim, client, &message, now),
            ALLOCATE | REFRESH | CREATE_PERMISSION | CHANNEL_BIND => {
                let answer = self.request(sim, client, &message, now);
                sim.send(self.listener, client, TTL, &answer);
            }
            _ => {}
        }
    }

    /// Relay a datagram toward `peer`, unless it is toward loopback, which
    /// destroys the allocation that sent it.
    fn relay_to_peer(
        &mut self,
        sim: &mut Sim,
        client: SocketAddr,
        peer: SocketAddr,
        data: &[u8],
        now: f64,
    ) -> bool {
        let Some(allocation) = self.live(client, now) else {
            return false;
        };
        if peer.ip().is_loopback() {
            allocation.alive = false;
            self.counts.destroyed += 1;
            return false;
        }
        if !allocation.permitted(peer.ip(), now) {
            self.counts.unpermitted += 1;
            return false;
        }
        let host = allocation.host;
        sim.send(host, peer, TTL, data);
        true
    }

    fn send_from_client(&mut self, sim: &mut Sim, client: SocketAddr, message: &Message, now: f64) {
        let (Some(peer), Some(data)) = (
            message
                .get(XOR_PEER_ADDRESS)
                .and_then(|value| xor_decode(value, &message.tid)),
            message.get(DATA),
        ) else {
            return;
        };
        if self.relay_to_peer(sim, client, peer, data, now) {
            self.counts.indications_in += 1;
            self.counts.largest_indication_in =
                self.counts.largest_indication_in.max(message.bytes.len());
        }
    }

    fn channel_from_client(&mut self, sim: &mut Sim, client: SocketAddr, bytes: &[u8], now: f64) {
        if bytes.len() < 4 {
            return;
        }
        let number = u16::from_be_bytes([bytes[0], bytes[1]]);
        let len = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        let Some(data) = bytes.get(4..4 + len) else {
            return;
        };
        let Some(peer) = self
            .live(client, now)
            .and_then(|a| a.channel_peer(number, now))
        else {
            return;
        };
        if self.relay_to_peer(sim, client, peer, data, now) {
            self.counts.channel_in += 1;
            self.counts.largest_channel_in = self.counts.largest_channel_in.max(bytes.len());
        }
    }

    fn peer_sent(&mut self, sim: &mut Sim, arrival: &Arrival) {
        let now = sim.now_ms();
        let Some(allocation) = self
            .allocations
            .iter()
            .find(|a| a.host == arrival.host && a.alive && now < a.expires_ms)
        else {
            return;
        };
        if !allocation.permitted(arrival.from.ip(), now) {
            self.counts.unpermitted += 1;
            return;
        }
        let client = allocation.client;
        let framed = match allocation.channel_to(arrival.from, now) {
            Some(number) => {
                let mut out = number.to_be_bytes().to_vec();
                out.extend_from_slice(&be16(arrival.bytes.len()));
                out.extend_from_slice(&arrival.bytes);
                self.counts.channel_out += 1;
                self.counts.largest_channel_out = self.counts.largest_channel_out.max(out.len());
                out
            }
            None => {
                self.next_tid = self.next_tid.wrapping_add(1);
                let mut tid = [0x7E; 12];
                tid[8..].copy_from_slice(&self.next_tid.to_be_bytes());
                let mut out = header(DATA_INDICATION, &tid);
                attribute(&mut out, DATA, &arrival.bytes);
                attribute(&mut out, XOR_PEER_ADDRESS, &xor_encode(arrival.from, &tid));
                let length = out.len() - 20;
                set_length(&mut out, length);
                self.counts.indications_out += 1;
                self.counts.largest_indication_out =
                    self.counts.largest_indication_out.max(out.len());
                out
            }
        };
        sim.send(self.listener, client, TTL, &framed);
    }

    /// Answer a request: the credentials checked, then the method.
    fn request(
        &mut self,
        sim: &mut Sim,
        client: SocketAddr,
        message: &Message,
        now: f64,
    ) -> Vec<u8> {
        let nonce = self.nonce(now);
        let (Some(username), Some(_), Some(sent_nonce), Some(_)) = (
            message.get(USERNAME),
            message.get(REALM),
            message.get(NONCE),
            message.integrity_at,
        ) else {
            self.counts.challenges += 1;
            return self.error(message, 401, Some(&nonce));
        };
        if sent_nonce != nonce.as_slice() {
            self.counts.stale_nonces += 1;
            return self.error(message, 438, Some(&nonce));
        }
        if username != self.username.as_bytes() || !self.verify(message) {
            self.counts.challenges += 1;
            return self.error(message, 401, Some(&nonce));
        }
        match message.kind {
            ALLOCATE => self.allocate(sim, client, message, now),
            REFRESH => self.refresh(client, message, now),
            CREATE_PERMISSION => self.create_permission(client, message, now),
            _ => self.channel_bind(client, message, now),
        }
    }

    fn verify(&self, message: &Message) -> bool {
        let Some(at) = message.integrity_at else {
            return false;
        };
        let mut covered = message.bytes[..at].to_vec();
        // The length the digest covers ends with the integrity attribute.
        set_length(&mut covered, at + 24 - 20);
        let mut mac = Hmac::<Sha1>::new_from_slice(&self.key).expect("any key length");
        mac.update(&covered);
        mac.verify_slice(&message.bytes[at + 4..at + 24]).is_ok()
    }

    fn allocate(
        &mut self,
        sim: &mut Sim,
        client: SocketAddr,
        message: &Message,
        now: f64,
    ) -> Vec<u8> {
        if let Some(existing) = self.live(client, now) {
            // A re-sent request is answered again; a second allocation on the
            // same flow is refused.
            if existing.tid != message.tid {
                return self.error(message, 437, None);
            }
            let (relayed, lifetime_s) = (existing.relayed, existing.lifetime_s);
            return self.allocated(message, client, relayed, lifetime_s);
        }
        if message
            .get(REQUESTED_TRANSPORT)
            .and_then(|value| value.first())
            != Some(&17)
        {
            return self.error(message, 442, None);
        }
        let lifetime_s = requested_lifetime(message)
            .filter(|&seconds| seconds > 0)
            .unwrap_or(DEFAULT_LIFETIME_S)
            .min(MAX_LIFETIME_S);
        let relayed = SocketAddr::new(self.relay_ip, self.next_port);
        self.next_port += 1;
        let host = sim.add_host(relayed, &self.chain);
        self.allocations.push(Allocation {
            client,
            tid: message.tid,
            relayed,
            host,
            lifetime_s,
            expires_ms: now + f64::from(lifetime_s) * 1000.0,
            alive: true,
            permissions: Vec::new(),
            channels: Vec::new(),
        });
        self.counts.allocations += 1;
        self.allocated(message, client, relayed, lifetime_s)
    }

    fn allocated(
        &self,
        message: &Message,
        client: SocketAddr,
        relayed: SocketAddr,
        lifetime_s: u32,
    ) -> Vec<u8> {
        self.success(
            message,
            &[
                (XOR_RELAYED_ADDRESS, xor_encode(relayed, &message.tid)),
                (XOR_MAPPED_ADDRESS, xor_encode(client, &message.tid)),
                (LIFETIME, lifetime_s.to_be_bytes().to_vec()),
            ],
        )
    }

    fn refresh(&mut self, client: SocketAddr, message: &Message, now: f64) -> Vec<u8> {
        let requested = requested_lifetime(message).unwrap_or(DEFAULT_LIFETIME_S);
        let Some(allocation) = self.live(client, now) else {
            return self.error(message, 437, None);
        };
        if requested == 0 {
            allocation.alive = false;
            self.counts.releases += 1;
            return self.success(message, &[(LIFETIME, 0u32.to_be_bytes().to_vec())]);
        }
        let lifetime_s = requested.min(MAX_LIFETIME_S);
        allocation.expires_ms = now + f64::from(lifetime_s) * 1000.0;
        allocation.lifetime_s = lifetime_s;
        self.counts.refreshes += 1;
        self.success(message, &[(LIFETIME, lifetime_s.to_be_bytes().to_vec())])
    }

    fn create_permission(&mut self, client: SocketAddr, message: &Message, now: f64) -> Vec<u8> {
        let peers: Vec<IpAddr> = message
            .attributes
            .iter()
            .filter(|(kind, _)| *kind == XOR_PEER_ADDRESS)
            .filter_map(|(_, value)| xor_decode(value, &message.tid))
            .map(|peer| peer.ip())
            .collect();
        if peers.is_empty() {
            return self.error(message, 400, None);
        }
        if peers.iter().any(|ip| self.refused.contains(ip)) {
            return self.error(message, 403, None);
        }
        let Some(allocation) = self.live(client, now) else {
            return self.error(message, 437, None);
        };
        for ip in peers {
            allocation.permit(ip, now);
        }
        self.counts.permissions += 1;
        self.success(message, &[])
    }

    fn channel_bind(&mut self, client: SocketAddr, message: &Message, now: f64) -> Vec<u8> {
        let number = message
            .get(CHANNEL_NUMBER)
            .filter(|value| value.len() == 4)
            .map(|value| u16::from_be_bytes([value[0], value[1]]));
        let peer = message
            .get(XOR_PEER_ADDRESS)
            .and_then(|value| xor_decode(value, &message.tid));
        let (Some(number), Some(peer)) = (number, peer) else {
            return self.error(message, 400, None);
        };
        if !(0x4000..=0x4FFF).contains(&number) {
            return self.error(message, 400, None);
        }
        if !self.binds_channels || self.refused.contains(&peer.ip()) {
            return self.error(message, 403, None);
        }
        // A number stays with its peer, and a peer with its number.
        let clash = match self.live(client, now) {
            None => return self.error(message, 437, None),
            Some(allocation) => allocation
                .channels
                .iter()
                .any(|(bound, bound_peer, until)| {
                    now < *until && ((*bound == number) != (*bound_peer == peer))
                }),
        };
        if clash {
            return self.error(message, 400, None);
        }
        let Some(allocation) = self.live(client, now) else {
            return self.error(message, 437, None);
        };
        allocation.channels.retain(|(bound, _, _)| *bound != number);
        allocation.channels.push((number, peer, now + CHANNEL_MS));
        allocation.permit(peer.ip(), now);
        self.counts.binds += 1;
        self.success(message, &[])
    }

    fn success(&self, message: &Message, attributes: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut out = header(message.kind | SUCCESS, &message.tid);
        for (kind, value) in attributes {
            attribute(&mut out, *kind, value);
        }
        let length = out.len() - 20 + 24;
        set_length(&mut out, length);
        let mut mac = Hmac::<Sha1>::new_from_slice(&self.key).expect("any key length");
        mac.update(&out);
        attribute(&mut out, MESSAGE_INTEGRITY, &mac.finalize().into_bytes());
        out
    }

    /// A refusal. A challenge carries the realm and the nonce to answer it
    /// with; nothing carries integrity.
    fn error(&self, message: &Message, code: u16, nonce: Option<&[u8]>) -> Vec<u8> {
        let mut out = header(message.kind | ERROR, &message.tid);
        attribute(&mut out, ERROR_CODE, &error_code(code));
        if let Some(nonce) = nonce {
            attribute(&mut out, NONCE, nonce);
            attribute(&mut out, REALM, &self.realm);
        }
        let length = out.len() - 20;
        set_length(&mut out, length);
        out
    }
}

fn requested_lifetime(message: &Message) -> Option<u32> {
    let value: [u8; 4] = message.get(LIFETIME)?.try_into().ok()?;
    Some(u32::from_be_bytes(value))
}
