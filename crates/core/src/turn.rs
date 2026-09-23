//! Relay codec: what a client asks of a relay, and what the relay sends back.
//!
//! The relay is the client's (docs/03-connectivity.md 7), and this is the
//! whole of its wire: the requests, the answers, and the two framings a
//! datagram to or from a peer travels in. Pure, as the check codec is: bytes
//! in, bytes out, no clock, no allocation, and transaction identifiers from the
//! caller. When to send what belongs to the relay's state machine; this module
//! knows only the shapes.
//!
//! Five properties of the wire are load bearing and none of them are obvious.
//!
//! **Nothing a relay sends is shaped like a check.** Its answers, the
//! datagrams it relays and its channel messages all fall on the record side of
//! the first-two-bytes rule (docs/01-protocol.md 2), so a datagram from the
//! relay is recognised by where it came from, before anything is classified.
//!
//! **The class is two bits, and they are not adjacent.** A success sets one of
//! them and an error sets both, so an error carries the success bit as well.
//! Read through that bit alone, every refusal is a success, and a refused
//! renewal goes unnoticed until what it renewed lapses.
//!
//! **A stale nonce is a challenge.** A relay rotates its nonce and answers the
//! next request with an error that carries the new one. Read as a plain
//! refusal, the first rotation ends the session.
//!
//! **The key is long-term.** Integrity is keyed by a digest of the username,
//! the relay's realm and the password, not by the password, so a new realm is
//! a new key.
//!
//! **Nothing after the integrity attribute is authenticated.** An answer is
//! read up to it and no further.

use core::fmt;
use core::net::{IpAddr, SocketAddr};

use md5::{Digest, Md5};
use zeroize::Zeroize;

use crate::error::{Error, Result};
use crate::stun::{
    self, ATTR_MESSAGE_INTEGRITY, ATTR_USERNAME, Attributes, HEADER_LEN, MAGIC_COOKIE,
    TransactionId, be16, be32, put_be16,
};

/// Channel data framing: the channel number, then the length.
pub const CHANNEL_HEADER_LEN: usize = 4;

/// The first channel number a client may bind.
pub const FIRST_CHANNEL: u16 = 0x4000;
/// The last.
pub const LAST_CHANNEL: u16 = 0x4FFF;

/// Longest realm this side holds. The standard bounds a realm and a nonce at
/// fewer than 128 characters, and relays send both in ASCII.
pub const MAX_REALM: usize = 128;
/// Longest nonce this side holds, for the same reason.
pub const MAX_NONCE: usize = 128;

/// The answer to a request without credentials, or with the wrong ones.
pub const UNAUTHORIZED: u16 = 401;
/// The nonce rotated; the answer carries the new one.
pub const STALE_NONCE: u16 = 438;

/// The transport a relay is asked for, by protocol number: datagrams.
const TRANSPORT_UDP: u8 = 17;

const METHOD_ALLOCATE: u16 = 0x0003;
const METHOD_REFRESH: u16 = 0x0004;
const METHOD_SEND: u16 = 0x0006;
const METHOD_DATA: u16 = 0x0007;
const METHOD_CREATE_PERMISSION: u16 = 0x0008;
const METHOD_CHANNEL_BIND: u16 = 0x0009;

/// The two class bits. Not adjacent, and an error sets both.
const CLASS_MASK: u16 = 0x0110;
const CLASS_REQUEST: u16 = 0x0000;
const CLASS_INDICATION: u16 = 0x0010;
const CLASS_SUCCESS: u16 = 0x0100;
const CLASS_ERROR: u16 = 0x0110;

const ATTR_ERROR_CODE: u16 = 0x0009;
const ATTR_CHANNEL_NUMBER: u16 = 0x000C;
const ATTR_LIFETIME: u16 = 0x000D;
const ATTR_XOR_PEER_ADDRESS: u16 = 0x0012;
const ATTR_DATA: u16 = 0x0013;
const ATTR_REALM: u16 = 0x0014;
const ATTR_NONCE: u16 = 0x0015;
const ATTR_XOR_RELAYED_ADDRESS: u16 = 0x0016;
const ATTR_REQUESTED_TRANSPORT: u16 = 0x0019;

/// The integrity attribute's value: an HMAC-SHA1 digest.
const DIGEST_LEN: usize = 20;

/// The long-term key: a digest of the username, the realm and the password.
///
/// Derived once per realm. Cleared when dropped, and never rendered.
pub struct Key([u8; 16]);

impl Key {
    /// The key a relay with `realm` checks our integrity against.
    ///
    /// The password is taken as given. The standard normalises it first, which
    /// changes nothing in the printable ASCII relays are configured with.
    pub fn long_term(username: &str, realm: &[u8], password: &str) -> Self {
        let mut digest = Md5::new();
        digest.update(username.as_bytes());
        digest.update(b":");
        digest.update(realm);
        digest.update(b":");
        digest.update(password.as_bytes());
        Self(digest.finalize().into())
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never render key material, not even indirectly.
        f.write_str("Key(..)")
    }
}

/// What an authenticated request carries: who we are, the realm and nonce the
/// relay's last challenge gave, and the key for that realm.
#[derive(Clone, Copy)]
pub struct Auth<'a> {
    pub username: &'a str,
    pub realm: &'a [u8],
    pub nonce: &'a [u8],
    pub key: &'a Key,
}

impl fmt::Debug for Auth<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The username is half a credential; none of this is rendered.
        f.write_str("Auth(..)")
    }
}

/// Build an allocation request into `out`, asking for `lifetime_s`.
///
/// Without `auth` this is a session's first request. It carries no
/// credentials, and its answer is the challenge that supplies what the next
/// one needs.
pub fn encode_allocate(
    out: &mut [u8],
    tid: TransactionId,
    lifetime_s: u32,
    auth: Option<&Auth<'_>>,
) -> Result<usize> {
    let mut w = Builder::new(out, METHOD_ALLOCATE | CLASS_REQUEST, tid)?;
    w.attribute(ATTR_REQUESTED_TRANSPORT, &[TRANSPORT_UDP, 0, 0, 0])?;
    w.attribute(ATTR_LIFETIME, &lifetime_s.to_be_bytes())?;
    match auth {
        Some(auth) => w.seal(auth),
        None => w.finish(),
    }
}

/// Build a refresh into `out`. A zero lifetime releases the allocation.
pub fn encode_refresh(
    out: &mut [u8],
    tid: TransactionId,
    lifetime_s: u32,
    auth: &Auth<'_>,
) -> Result<usize> {
    let mut w = Builder::new(out, METHOD_REFRESH | CLASS_REQUEST, tid)?;
    w.attribute(ATTR_LIFETIME, &lifetime_s.to_be_bytes())?;
    w.seal(auth)
}

/// Build a permission request for `peer` into `out`.
///
/// A permission is an address's, not a port's: the port is written as zero,
/// and a relay ignores it. One address a request, so a refusal refuses that
/// address and no other.
pub fn encode_create_permission(
    out: &mut [u8],
    tid: TransactionId,
    peer: IpAddr,
    auth: &Auth<'_>,
) -> Result<usize> {
    let mut w = Builder::new(out, METHOD_CREATE_PERMISSION | CLASS_REQUEST, tid)?;
    w.address(ATTR_XOR_PEER_ADDRESS, SocketAddr::new(peer, 0))?;
    w.seal(auth)
}

/// Build a request binding `channel` to `peer` into `out`.
pub fn encode_channel_bind(
    out: &mut [u8],
    tid: TransactionId,
    channel: u16,
    peer: SocketAddr,
    auth: &Auth<'_>,
) -> Result<usize> {
    check_channel(channel)?;
    let mut w = Builder::new(out, METHOD_CHANNEL_BIND | CLASS_REQUEST, tid)?;
    let [high, low] = channel.to_be_bytes();
    w.attribute(ATTR_CHANNEL_NUMBER, &[high, low, 0, 0])?;
    w.address(ATTR_XOR_PEER_ADDRESS, peer)?;
    w.seal(auth)
}

/// Bytes an indication puts ahead of a datagram bound for `peer`: the header,
/// the peer's address and the data attribute's own header. 36 toward an IPv4
/// peer and 48 toward an IPv6 one, with padding to four bytes after the
/// datagram.
pub fn indication_header_len(peer: SocketAddr) -> usize {
    let address = match stun::canonical(peer) {
        SocketAddr::V4(_) => 8,
        SocketAddr::V6(_) => 20,
    };
    HEADER_LEN + 4 + address + 4
}

/// Frame, in place, the `len` bytes the caller already wrote at
/// `out[indication_header_len(peer)..]` as an indication toward `peer`.
///
/// The datagram is written first and framed afterwards, so nothing is copied:
/// the header goes in front of it and the padding after. Returns the framed
/// length.
pub fn wrap_indication(
    out: &mut [u8],
    tid: TransactionId,
    peer: SocketAddr,
    len: usize,
) -> Result<usize> {
    let mut w = Builder::new(out, METHOD_SEND | CLASS_INDICATION, tid)?;
    w.address(ATTR_XOR_PEER_ADDRESS, peer)?;
    w.attribute_header(ATTR_DATA, len)?;
    w.skip(len)?;
    w.pad(len)?;
    w.finish()
}

/// Frame, in place, the `len` bytes the caller already wrote at
/// `out[CHANNEL_HEADER_LEN..]` as channel data on `channel`.
///
/// Unpadded: over datagrams the padding is optional, and a relay delivers
/// either form.
pub fn wrap_channel(out: &mut [u8], channel: u16, len: usize) -> Result<usize> {
    check_channel(channel)?;
    let total = CHANNEL_HEADER_LEN
        .checked_add(len)
        .ok_or(Error::Oversized)?;
    if total > out.len() {
        return Err(Error::BufferTooSmall);
    }
    put_be16(out, 0, channel)?;
    put_be16(out, 2, u16::try_from(len).map_err(|_| Error::Oversized)?)?;
    Ok(total)
}

fn check_channel(channel: u16) -> Result<()> {
    if (FIRST_CHANNEL..=LAST_CHANNEL).contains(&channel) {
        Ok(())
    } else {
        Err(Error::Malformed)
    }
}

/// What a datagram from the relay turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inbound<'a> {
    /// The relay answered one of our requests.
    Response(Response<'a>),
    /// A peer's datagram, relayed with the peer's address beside it.
    Indication { peer: SocketAddr, data: &'a [u8] },
    /// A peer's datagram, relayed on a channel. Which peer is the binding's.
    Channel { number: u16, data: &'a [u8] },
}

/// Which request an answer is to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    Allocate,
    Refresh,
    CreatePermission,
    ChannelBind,
}

/// Classify and structurally validate a datagram that came from the relay.
///
/// Refuses what a relay has no reason to send a client: a request, or an
/// indication other than a relayed datagram. Authenticates nothing. An
/// answer's integrity is checked against the key the caller holds
/// ([`Response::verify`]), and a relayed datagram is the peer's to
/// authenticate, not the relay's.
pub fn parse(datagram: &[u8]) -> Result<Inbound<'_>> {
    // A message begins with its two top bits clear, channel data with 0b01.
    match datagram.first().map(|byte| byte >> 6) {
        Some(0b00) => parse_message(datagram),
        Some(0b01) => parse_channel(datagram),
        _ => Err(Error::Malformed),
    }
}

fn parse_channel(datagram: &[u8]) -> Result<Inbound<'_>> {
    let number = be16(datagram, 0)?;
    let len = usize::from(be16(datagram, 2)?);
    let data = datagram
        .get(CHANNEL_HEADER_LEN..CHANNEL_HEADER_LEN + len)
        .ok_or(Error::Malformed)?;
    // Padded to four bytes or not at all; anything longer is not this message.
    if datagram.len() > CHANNEL_HEADER_LEN + len.next_multiple_of(4) {
        return Err(Error::Malformed);
    }
    Ok(Inbound::Channel { number, data })
}

fn parse_message(datagram: &[u8]) -> Result<Inbound<'_>> {
    let head = datagram.get(..HEADER_LEN).ok_or(Error::Malformed)?;
    let kind = be16(head, 0)?;
    if be32(head, 4)? != MAGIC_COOKIE {
        return Err(Error::Malformed);
    }
    // The length must land exactly on the end of the datagram, as a check's
    // must: a reader that trusted a short one would act on bytes the integrity
    // never covered.
    let claimed = usize::from(be16(head, 2)?);
    if claimed != datagram.len() - HEADER_LEN {
        return Err(Error::Malformed);
    }
    let mut tid = [0u8; 12];
    tid.copy_from_slice(head.get(8..HEADER_LEN).ok_or(Error::Malformed)?);
    let tid = TransactionId(tid);

    // One walk proves the structure, finds the integrity attribute and takes
    // what the frequent messages need. The first of each kind counts, and
    // nothing after the integrity attribute is read.
    let mut walk = Attributes::new(datagram, datagram.len());
    let mut reached = HEADER_LEN;
    let mut integrity_at = None;
    let (mut peer, mut data, mut error) = (None, None, None);
    while let Some(attribute) = walk.next() {
        let (attr, value, next) = attribute?;
        if integrity_at.is_none() {
            match attr {
                ATTR_MESSAGE_INTEGRITY if value.len() == DIGEST_LEN => integrity_at = Some(reached),
                ATTR_MESSAGE_INTEGRITY => return Err(Error::Malformed),
                ATTR_XOR_PEER_ADDRESS if peer.is_none() => peer = Some(value),
                ATTR_DATA if data.is_none() => data = Some(value),
                ATTR_ERROR_CODE if error.is_none() => error = Some(value),
                _ => {}
            }
        }
        reached = next;
    }
    if reached != datagram.len() {
        return Err(Error::Malformed);
    }

    match (kind & !CLASS_MASK, kind & CLASS_MASK) {
        // The two attributes may come in either order.
        (METHOD_DATA, CLASS_INDICATION) => Ok(Inbound::Indication {
            peer: peer
                .and_then(|value| stun::decode_mapped(value, tid))
                .ok_or(Error::Malformed)?,
            data: data.ok_or(Error::Malformed)?,
        }),
        (method, class @ (CLASS_SUCCESS | CLASS_ERROR)) => {
            let method = match method {
                METHOD_ALLOCATE => Method::Allocate,
                METHOD_REFRESH => Method::Refresh,
                METHOD_CREATE_PERMISSION => Method::CreatePermission,
                METHOD_CHANNEL_BIND => Method::ChannelBind,
                _ => return Err(Error::Malformed),
            };
            // An error that does not say which is refused here, rather than
            // read as some default the caller would then act on.
            let error = match class {
                CLASS_ERROR => Some(error.and_then(error_code).ok_or(Error::Malformed)?),
                _ => None,
            };
            Ok(Inbound::Response(Response {
                bytes: datagram,
                method,
                tid,
                error,
                integrity_at,
            }))
        }
        _ => Err(Error::Malformed),
    }
}

/// Read an error code: its hundreds in the low three bits of the third byte,
/// the rest in the fourth. The reason phrase after them is not needed.
fn error_code(value: &[u8]) -> Option<u16> {
    let hundreds = u16::from(*value.get(2)? & 0x07);
    let rest = u16::from(*value.get(3)?);
    ((3..=6).contains(&hundreds) && rest < 100).then_some(hundreds * 100 + rest)
}

/// An answer to one of our requests, borrowing the datagram it came in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response<'a> {
    bytes: &'a [u8],
    method: Method,
    tid: TransactionId,
    /// The error code, or `None` for a success.
    error: Option<u16>,
    /// Offset of the integrity attribute, where the hash and all reading stop.
    integrity_at: Option<usize>,
}

impl<'a> Response<'a> {
    /// Which request this answers.
    pub fn method(&self) -> Method {
        self.method
    }

    /// The identifier to match against an outstanding request.
    pub fn transaction_id(&self) -> TransactionId {
        self.tid
    }

    /// True for a success, read through both class bits.
    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }

    /// The code a refusal carries, or `None` for a success.
    pub fn error_code(&self) -> Option<u16> {
        self.error
    }

    /// The realm and nonce to authenticate with, if this answer is a challenge.
    ///
    /// The first challenge and a stale nonce both are: each carries the nonce
    /// the request goes again with. A realm or nonce too long to hold is no
    /// challenge this side can answer.
    pub fn challenge(&self) -> Option<Challenge<'a>> {
        if !matches!(self.error, Some(UNAUTHORIZED | STALE_NONCE)) {
            return None;
        }
        let realm = self
            .find(ATTR_REALM)
            .filter(|realm| realm.len() <= MAX_REALM)?;
        let nonce = self
            .find(ATTR_NONCE)
            .filter(|nonce| !nonce.is_empty() && nonce.len() <= MAX_NONCE)?;
        Some(Challenge { realm, nonce })
    }

    /// The address the relay allocated, from an allocation's success.
    pub fn relayed_address(&self) -> Option<SocketAddr> {
        stun::decode_mapped(self.find(ATTR_XOR_RELAYED_ADDRESS)?, self.tid)
    }

    /// The lifetime the relay granted, in seconds, if it said.
    pub fn lifetime_s(&self) -> Option<u32> {
        let value: [u8; 4] = self.find(ATTR_LIFETIME)?.try_into().ok()?;
        Some(u32::from_be_bytes(value))
    }

    /// True if the answer carries integrity at all. A challenge and a refusal
    /// carry none.
    pub fn is_authenticated(&self) -> bool {
        self.integrity_at.is_some()
    }

    /// True if the answer authenticates under `key`.
    ///
    /// An answer without integrity never satisfies this, whatever the key.
    pub fn verify(&self, key: &Key) -> bool {
        let Some(at) = self.integrity_at else {
            return false;
        };
        let Ok(mac) = stun::integrity_of(self.bytes, at, &key.0) else {
            return false;
        };
        let Some(carried) = self.bytes.get(at + 4..at + 4 + DIGEST_LEN) else {
            return false;
        };
        // Constant-time: a forged answer must not be distinguishable by how
        // long the comparison took.
        let mut diff = 0u8;
        for (a, b) in mac.iter().zip(carried.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// The first attribute of `kind` ahead of the integrity attribute.
    fn find(&self, kind: u16) -> Option<&'a [u8]> {
        let end = self.integrity_at.unwrap_or(self.bytes.len());
        let mut walk = Attributes::new(self.bytes, end);
        while let Some(Ok((attr, value, _))) = walk.next() {
            if attr == kind {
                return Some(value);
            }
        }
        None
    }
}

/// What a challenge supplies: the realm the key is derived for, and the nonce
/// every request carries until the next challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Challenge<'a> {
    pub realm: &'a [u8],
    pub nonce: &'a [u8],
}

/// Writes a message front to back, then its length.
struct Builder<'a> {
    buf: &'a mut [u8],
    at: usize,
    tid: TransactionId,
}

impl<'a> Builder<'a> {
    fn new(buf: &'a mut [u8], kind: u16, tid: TransactionId) -> Result<Self> {
        let mut w = Self { buf, at: 0, tid };
        w.put(&kind.to_be_bytes())?;
        // The length is written last, once it is known.
        w.put(&0u16.to_be_bytes())?;
        w.put(&MAGIC_COOKIE.to_be_bytes())?;
        w.put(&tid.0)?;
        Ok(w)
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.at.checked_add(bytes.len()).ok_or(Error::Oversized)?;
        self.buf
            .get_mut(self.at..end)
            .ok_or(Error::BufferTooSmall)?
            .copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }

    /// Step over `len` bytes the caller already wrote in place.
    fn skip(&mut self, len: usize) -> Result<()> {
        let end = self.at.checked_add(len).ok_or(Error::Oversized)?;
        if end > self.buf.len() {
            return Err(Error::BufferTooSmall);
        }
        self.at = end;
        Ok(())
    }

    fn attribute_header(&mut self, kind: u16, len: usize) -> Result<()> {
        let len = u16::try_from(len).map_err(|_| Error::Oversized)?;
        self.put(&kind.to_be_bytes())?;
        self.put(&len.to_be_bytes())
    }

    /// Pad a value of `len` bytes out to a four-byte boundary.
    fn pad(&mut self, len: usize) -> Result<()> {
        let zeros = [0u8; 3];
        self.put(zeros.get(..(4 - len % 4) % 4).ok_or(Error::Malformed)?)
    }

    fn attribute(&mut self, kind: u16, value: &[u8]) -> Result<()> {
        self.attribute_header(kind, value.len())?;
        self.put(value)?;
        self.pad(value.len())
    }

    fn address(&mut self, kind: u16, addr: SocketAddr) -> Result<()> {
        let mut value = [0u8; 20];
        let len = stun::encode_mapped(&mut value, addr, self.tid)?;
        self.attribute(kind, value.get(..len).ok_or(Error::BufferTooSmall)?)
    }

    /// Append the credentials, then integrity over everything before it, and
    /// finish.
    fn seal(mut self, auth: &Auth<'_>) -> Result<usize> {
        self.attribute(ATTR_USERNAME, auth.username.as_bytes())?;
        self.attribute(ATTR_NONCE, auth.nonce)?;
        self.attribute(ATTR_REALM, auth.realm)?;
        let at = self.at;
        self.attribute_header(ATTR_MESSAGE_INTEGRITY, DIGEST_LEN)?;
        let mac = stun::integrity_of(self.buf, at, &auth.key.0)?;
        self.put(&mac)?;
        self.finish()
    }

    /// Write the length, which counts everything after the header.
    fn finish(self) -> Result<usize> {
        let len = u16::try_from(self.at - HEADER_LEN).map_err(|_| Error::Oversized)?;
        put_be16(self.buf, 2, len)?;
        Ok(self.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{self, Datagram};
    use core::net::{Ipv4Addr, Ipv6Addr};
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    use std::vec::Vec;

    const TID: TransactionId = TransactionId([
        0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87, 0x98, 0xA9, 0xBA, 0xCB,
    ]);
    const USER: &str = "user";
    const PASS: &str = "password";
    const REALM: &[u8] = b"relay.example";
    const NONCE: &[u8] = b"5d1b0a4f3c2e7a90";

    fn key() -> Key {
        Key::long_term(USER, REALM, PASS)
    }

    fn peer_v4() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)), 50_123)
    }

    fn peer_v6() -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6("2001:db8::7".parse::<Ipv6Addr>().unwrap()),
            50_123,
        )
    }

    /// An address attribute's value, obfuscated as the standard lays it out.
    /// Written here rather than through the codec, so a misreading the two
    /// shared could not pass.
    fn xor_address(addr: SocketAddr, tid: TransactionId) -> Vec<u8> {
        let mut mask = MAGIC_COOKIE.to_be_bytes().to_vec();
        mask.extend_from_slice(&tid.0);
        let (family, octets) = match addr.ip() {
            IpAddr::V4(ip) => (0x01, ip.octets().to_vec()),
            IpAddr::V6(ip) => (0x02, ip.octets().to_vec()),
        };
        let mut value = std::vec![0, family];
        value.extend_from_slice(&(addr.port() ^ 0x2112).to_be_bytes());
        value.extend(octets.iter().zip(mask.iter()).map(|(a, b)| a ^ b));
        value
    }

    /// Append an attribute and correct the length.
    fn append(out: &mut Vec<u8>, attr: u16, value: &[u8]) {
        out.extend_from_slice(&attr.to_be_bytes());
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value);
        out.resize(out.len().next_multiple_of(4), 0);
        let length = (out.len() - HEADER_LEN) as u16;
        out[2..4].copy_from_slice(&length.to_be_bytes());
    }

    /// A message as a relay lays it out: the header, the attributes in the
    /// order given, and integrity under `key` when there is one. Written from
    /// the layout rather than through the codec, for the same reason.
    fn message(kind: u16, attributes: &[(u16, &[u8])], key: Option<&Key>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&kind.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&MAGIC_COOKIE.to_be_bytes());
        out.extend_from_slice(&TID.0);
        for (attr, value) in attributes {
            append(&mut out, *attr, value);
        }
        if let Some(key) = key {
            // The digest covers a length that already counts its own attribute.
            let length = (out.len() - HEADER_LEN + 24) as u16;
            out[2..4].copy_from_slice(&length.to_be_bytes());
            let mut mac = Hmac::<Sha1>::new_from_slice(&key.0).unwrap();
            mac.update(&out);
            let digest = mac.finalize().into_bytes();
            append(&mut out, ATTR_MESSAGE_INTEGRITY, &digest);
        }
        out
    }

    fn response(bytes: &[u8]) -> Response<'_> {
        match parse(bytes).unwrap() {
            Inbound::Response(response) => response,
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    /// A request read back attribute by attribute, from the layout.
    fn attributes_of(bytes: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let length = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        assert_eq!(
            length,
            bytes.len() - HEADER_LEN,
            "the length misses the end"
        );
        let mut at = HEADER_LEN;
        let mut out = Vec::new();
        while at < bytes.len() {
            let attr = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
            let len = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
            out.push((attr, bytes[at + 4..at + 4 + len].to_vec()));
            at += 4 + len.next_multiple_of(4);
        }
        assert_eq!(at, bytes.len(), "the attributes overran the message");
        out
    }

    /// True if a message that ends in its integrity attribute is right under
    /// `key`, computed from the layout.
    fn authenticates(bytes: &[u8], key: &Key) -> bool {
        let at = bytes.len() - 24;
        let mut mac = Hmac::<Sha1>::new_from_slice(&key.0).unwrap();
        mac.update(&bytes[..at]);
        bytes[at..at + 4] == [0x00, 0x08, 0x00, 0x14]
            && mac.finalize().into_bytes()[..] == bytes[at + 4..]
    }

    /// Assert a request's method, identifier and leading attributes, and that
    /// it ends in the credentials and integrity under `key`.
    fn assert_signed(bytes: &[u8], kind: u16, leading: &[(u16, Vec<u8>)], key: &Key) {
        assert_eq!(u16::from_be_bytes([bytes[0], bytes[1]]), kind);
        assert_eq!(&bytes[8..20], &TID.0);
        let mut expected = leading.to_vec();
        expected.push((ATTR_USERNAME, USER.as_bytes().to_vec()));
        expected.push((ATTR_NONCE, NONCE.to_vec()));
        expected.push((ATTR_REALM, REALM.to_vec()));
        let found = attributes_of(bytes);
        assert_eq!(found[..found.len() - 1], expected[..]);
        assert!(authenticates(bytes, key), "the integrity is wrong");
    }

    /// The standard's own sample of a request under long-term credentials.
    /// Reproducing it byte for byte pins the key, the layout of the
    /// credentials and the integrity together, against something no code here
    /// wrote.
    #[test]
    fn the_long_term_sample_is_reproduced_byte_for_byte() {
        const SAMPLE: [u8; 116] = [
            0x00, 0x01, 0x00, 0x60, 0x21, 0x12, 0xa4, 0x42, 0x78, 0xad, 0x34, 0x33, 0xc6, 0xad,
            0x72, 0xc0, 0x29, 0xda, 0x41, 0x2e, 0x00, 0x06, 0x00, 0x12, 0xe3, 0x83, 0x9e, 0xe3,
            0x83, 0x88, 0xe3, 0x83, 0xaa, 0xe3, 0x83, 0x83, 0xe3, 0x82, 0xaf, 0xe3, 0x82, 0xb9,
            0x00, 0x00, 0x00, 0x15, 0x00, 0x1c, 0x66, 0x2f, 0x2f, 0x34, 0x39, 0x39, 0x6b, 0x39,
            0x35, 0x34, 0x64, 0x36, 0x4f, 0x4c, 0x33, 0x34, 0x6f, 0x4c, 0x39, 0x46, 0x53, 0x54,
            0x76, 0x79, 0x36, 0x34, 0x73, 0x41, 0x00, 0x14, 0x00, 0x0b, 0x65, 0x78, 0x61, 0x6d,
            0x70, 0x6c, 0x65, 0x2e, 0x6f, 0x72, 0x67, 0x00, 0x00, 0x08, 0x00, 0x14, 0xf6, 0x70,
            0x24, 0x65, 0x6d, 0xd6, 0x4a, 0x3e, 0x02, 0xb8, 0xe0, 0x71, 0x2e, 0x85, 0xc9, 0xa2,
            0x8c, 0xa8, 0x96, 0x66,
        ];
        // Six katakana characters, in UTF-8.
        let username = core::str::from_utf8(&SAMPLE[24..42]).unwrap();
        // The password after the normalisation the sample applies, which is
        // the form a relay is configured with.
        let key = Key::long_term(username, b"example.org", "TheMatrIX");
        let auth = Auth {
            username,
            realm: b"example.org",
            nonce: b"f//499k954d6OL34oL9FSTvy64sA",
            key: &key,
        };
        let tid = TransactionId(SAMPLE[8..20].try_into().unwrap());

        // The sample is a binding request. What the credentials add is the
        // same for every method.
        let mut out = [0u8; 256];
        let len = Builder::new(&mut out, 0x0001, tid)
            .unwrap()
            .seal(&auth)
            .unwrap();
        assert_eq!(&out[..len], &SAMPLE[..]);
        assert!(authenticates(&SAMPLE, &key));
    }

    /// Every request, read back from its layout: the method, the identifier,
    /// each attribute's value, and integrity over all of it under the
    /// long-term key. The first allocation carries no credentials at all.
    #[test]
    fn every_request_is_laid_out_as_the_standard_says() {
        let key = key();
        let auth = Auth {
            username: USER,
            realm: REALM,
            nonce: NONCE,
            key: &key,
        };
        let transport = (ATTR_REQUESTED_TRANSPORT, std::vec![17, 0, 0, 0]);
        let lifetime = |seconds: u32| (ATTR_LIFETIME, seconds.to_be_bytes().to_vec());
        let mut out = [0u8; 512];

        let len = encode_allocate(&mut out, TID, 600, None).unwrap();
        assert_eq!(&out[..2], &[0x00, 0x03]);
        assert_eq!(&out[8..20], &TID.0);
        assert_eq!(
            attributes_of(&out[..len]),
            [transport.clone(), lifetime(600)]
        );

        let len = encode_allocate(&mut out, TID, 600, Some(&auth)).unwrap();
        assert_signed(&out[..len], 0x0003, &[transport, lifetime(600)], &key);

        // A zero lifetime is what releases an allocation, so it is carried as
        // written rather than left out.
        let len = encode_refresh(&mut out, TID, 0, &auth).unwrap();
        assert_signed(&out[..len], 0x0004, &[lifetime(0)], &key);

        let len = encode_create_permission(&mut out, TID, peer_v4().ip(), &auth).unwrap();
        let address = xor_address(SocketAddr::new(peer_v4().ip(), 0), TID);
        assert_signed(
            &out[..len],
            0x0008,
            &[(ATTR_XOR_PEER_ADDRESS, address)],
            &key,
        );

        for (channel, peer) in [(FIRST_CHANNEL, peer_v4()), (LAST_CHANNEL, peer_v6())] {
            let len = encode_channel_bind(&mut out, TID, channel, peer, &auth).unwrap();
            let mut number = channel.to_be_bytes().to_vec();
            number.extend_from_slice(&[0, 0]);
            assert_signed(
                &out[..len],
                0x0009,
                &[
                    (ATTR_CHANNEL_NUMBER, number),
                    (ATTR_XOR_PEER_ADDRESS, xor_address(peer, TID)),
                ],
                &key,
            );
        }

        // Outside the numbers a client may bind, nothing is built.
        for channel in [FIRST_CHANNEL - 1, LAST_CHANNEL + 1] {
            assert_eq!(
                encode_channel_bind(&mut out, TID, channel, peer_v4(), &auth),
                Err(Error::Malformed)
            );
        }
    }

    /// A datagram is written where it will travel and framed around it: the
    /// bytes in place are never touched, the padding after them is zeroed, and
    /// a floor-sized datagram toward an IPv4 peer comes to 1268 bytes. A relay
    /// delivers the same layout under the data method.
    #[test]
    fn a_datagram_is_framed_in_place() {
        for peer in [peer_v4(), peer_v6()] {
            let head = indication_header_len(peer);
            let payload: Vec<u8> = (0..crate::DEFAULT_DATAGRAM)
                .map(|i| (i * 7) as u8)
                .collect();
            let mut out = std::vec![0xEE_u8; 1400];
            out[head..head + payload.len()].copy_from_slice(&payload);

            let len = wrap_indication(&mut out, TID, peer, payload.len()).unwrap();
            assert_eq!(len, head + payload.len() + 3);
            assert_eq!(
                &out[head..head + payload.len()],
                &payload[..],
                "the datagram moved"
            );
            assert_eq!(&out[head + payload.len()..len], &[0, 0, 0]);
            assert_eq!(&out[..2], &[0x00, 0x16]);
            assert_eq!(
                attributes_of(&out[..len]),
                [
                    (ATTR_XOR_PEER_ADDRESS, xor_address(peer, TID)),
                    (ATTR_DATA, payload.clone())
                ]
            );

            out[1] = 0x17;
            assert_eq!(
                parse(&out[..len]),
                Ok(Inbound::Indication {
                    peer,
                    data: &payload[..]
                })
            );
            assert_eq!(
                wrap_indication(&mut out[..len - 1], TID, peer, payload.len()),
                Err(Error::BufferTooSmall)
            );
        }
        assert_eq!(indication_header_len(peer_v4()), 36);
        assert_eq!(indication_header_len(peer_v6()), 48);
        assert_eq!(
            indication_header_len(peer_v4()) + crate::DEFAULT_DATAGRAM + 3,
            1268
        );
    }

    /// Channel data is four bytes of framing and no padding: a floor-sized
    /// datagram comes to 1233 bytes.
    #[test]
    fn channel_data_is_framed_in_place_unpadded() {
        let payload = [0x5A_u8; crate::DEFAULT_DATAGRAM];
        let mut out = [0xEE_u8; 1300];
        out[CHANNEL_HEADER_LEN..CHANNEL_HEADER_LEN + payload.len()].copy_from_slice(&payload);

        let len = wrap_channel(&mut out, FIRST_CHANNEL, payload.len()).unwrap();
        assert_eq!(len, 1233);
        assert_eq!(&out[..4], &[0x40, 0x00, 0x04, 0xCD]);
        assert_eq!(
            parse(&out[..len]),
            Ok(Inbound::Channel {
                number: FIRST_CHANNEL,
                data: &payload[..]
            })
        );
        assert_eq!(
            wrap_channel(&mut out, FIRST_CHANNEL - 1, 4),
            Err(Error::Malformed)
        );
        assert_eq!(
            wrap_channel(&mut out[..8], FIRST_CHANNEL, 5),
            Err(Error::BufferTooSmall)
        );
    }

    /// A relay may pad channel data to four bytes or not; both are the same
    /// datagram. Anything past the padding is not, and neither is a datagram
    /// shorter than its length says.
    #[test]
    fn channel_data_is_read_padded_and_unpadded() {
        let payload = [0xAB_u8; 5];
        let mut unpadded = std::vec![0x40, 0x00, 0x00, 0x05];
        unpadded.extend_from_slice(&payload);
        let mut padded = unpadded.clone();
        padded.extend_from_slice(&[0, 0, 0]);

        for bytes in [&unpadded, &padded] {
            assert_eq!(
                parse(bytes),
                Ok(Inbound::Channel {
                    number: 0x4000,
                    data: &payload[..]
                })
            );
        }
        let mut overlong = padded.clone();
        overlong.push(0);
        assert_eq!(parse(&overlong), Err(Error::Malformed));
        assert_eq!(parse(&unpadded[..8]), Err(Error::Malformed));
    }

    /// The largest framing there is: a floor-sized datagram from an IPv6 peer
    /// arrives as 1280 bytes, all of which must be read, or every full-size
    /// datagram is lost while small ones pass.
    #[test]
    fn a_full_size_datagram_from_an_ipv6_peer_is_unwrapped_whole() {
        let payload: Vec<u8> = (0..crate::DEFAULT_DATAGRAM).map(|i| i as u8).collect();
        let bytes = message(
            0x0017,
            &[
                (ATTR_XOR_PEER_ADDRESS, &xor_address(peer_v6(), TID)),
                (ATTR_DATA, &payload),
            ],
            None,
        );
        assert_eq!(bytes.len(), 1280);
        assert_eq!(
            parse(&bytes),
            Ok(Inbound::Indication {
                peer: peer_v6(),
                data: &payload[..]
            })
        );
    }

    /// A relayed datagram's two attributes come in either order; a deployed
    /// relay puts the data first.
    #[test]
    fn a_relayed_datagram_is_read_in_either_order() {
        let payload = [0x17_u8, 0xFE, 0xFD, 0x00, 0x01];
        let address = xor_address(peer_v4(), TID);
        let peer = (ATTR_XOR_PEER_ADDRESS, &address[..]);
        let data = (ATTR_DATA, &payload[..]);
        for attributes in [[data, peer], [peer, data]] {
            assert_eq!(
                parse(&message(0x0017, &attributes, None)),
                Ok(Inbound::Indication {
                    peer: peer_v4(),
                    data: &payload[..]
                })
            );
        }
    }

    /// The answers a relay gives a client, laid out as a deployed relay lays
    /// them out: its attributes in its order, integrity last on a success and
    /// absent from a challenge or a refusal.
    #[test]
    fn the_answers_a_relay_gives_are_read() {
        let key = key();
        let relayed = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)), 50_048);
        let mapped = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9)), 41_000);

        let first = message(
            0x0113,
            &[
                (ATTR_ERROR_CODE, &[0, 0, 4, 1]),
                (ATTR_NONCE, NONCE),
                (ATTR_REALM, REALM),
            ],
            None,
        );
        let first = response(&first);
        assert_eq!(first.method(), Method::Allocate);
        assert_eq!(first.error_code(), Some(UNAUTHORIZED));
        assert!(!first.is_authenticated());
        assert_eq!(
            first.challenge(),
            Some(Challenge {
                realm: REALM,
                nonce: NONCE
            })
        );

        let allocated = message(
            0x0103,
            &[
                (ATTR_XOR_RELAYED_ADDRESS, &xor_address(relayed, TID)),
                (0x0020, &xor_address(mapped, TID)),
                (ATTR_LIFETIME, &600u32.to_be_bytes()),
            ],
            Some(&key),
        );
        let allocated = response(&allocated);
        assert!(allocated.is_success());
        assert_eq!(allocated.transaction_id(), TID);
        assert_eq!(allocated.relayed_address(), Some(relayed));
        assert_eq!(allocated.lifetime_s(), Some(600));
        assert!(allocated.verify(&key));

        let refreshed = message(
            0x0104,
            &[(ATTR_LIFETIME, &600u32.to_be_bytes())],
            Some(&key),
        );
        let permitted = message(0x0108, &[], Some(&key));
        let bound = message(0x0109, &[], Some(&key));
        for (bytes, method) in [
            (&refreshed, Method::Refresh),
            (&permitted, Method::CreatePermission),
            (&bound, Method::ChannelBind),
        ] {
            let granted = response(bytes);
            assert_eq!(granted.method(), method);
            assert!(granted.is_success() && granted.verify(&key));
        }

        // A second allocation on the same flow is refused with the code alone.
        let mismatch = message(0x0113, &[(ATTR_ERROR_CODE, &[0, 0, 4, 37])], None);
        assert_eq!(response(&mismatch).error_code(), Some(437));
        assert_eq!(response(&mismatch).challenge(), None);
    }

    /// An error sets both class bits, the success bit among them. Read through
    /// that bit alone, every refusal is a success, and a refused renewal goes
    /// unnoticed until what it renewed lapses.
    #[test]
    fn an_error_response_is_not_a_success() {
        const REFRESH_ERROR: u16 = 0x0114;
        assert_ne!(
            REFRESH_ERROR & 0x0100,
            0,
            "the error must carry the success bit, or this proves nothing"
        );

        let refused = message(REFRESH_ERROR, &[(ATTR_ERROR_CODE, &[0, 0, 4, 37])], None);
        let refused = response(&refused);
        assert_eq!(refused.method(), Method::Refresh);
        assert!(!refused.is_success());
        assert_eq!(refused.error_code(), Some(437));

        let granted = message(0x0104, &[(ATTR_LIFETIME, &600u32.to_be_bytes())], None);
        let granted = response(&granted);
        assert!(granted.is_success());
        assert_eq!(granted.error_code(), None);
        assert_eq!(granted.lifetime_s(), Some(600));
    }

    /// A relay rotates its nonce and answers the next request with a stale
    /// nonce error that carries the new one. It is a challenge like the first,
    /// or the first rotation ends the session.
    #[test]
    fn a_stale_nonce_is_a_challenge() {
        let rotated: &[u8] = b"a0b1c2d3e4f5a6b7";
        let stale = message(
            0x0118,
            &[
                (ATTR_ERROR_CODE, &[0, 0, 4, 38]),
                (ATTR_NONCE, rotated),
                (ATTR_REALM, REALM),
            ],
            None,
        );
        let stale = response(&stale);
        assert_eq!(stale.method(), Method::CreatePermission);
        assert_eq!(stale.error_code(), Some(STALE_NONCE));
        assert_eq!(
            stale.challenge(),
            Some(Challenge {
                realm: REALM,
                nonce: rotated
            })
        );

        // A refusal is no challenge, whatever it carries; nor is a challenge
        // with no nonce to answer it with, or one too long to hold.
        let long = [b'n'; MAX_NONCE + 1];
        for (code, nonce) in [(3u8, rotated), (38, &[][..]), (38, &long[..])] {
            let error = [0, 0, 4, code];
            let mut attributes = std::vec![(ATTR_ERROR_CODE, &error[..])];
            if !nonce.is_empty() {
                attributes.push((ATTR_NONCE, nonce));
            }
            attributes.push((ATTR_REALM, REALM));
            let bytes = message(0x0118, &attributes, None);
            assert_eq!(response(&bytes).challenge(), None, "code 4{code:02}");
        }
    }

    /// Integrity is keyed by the username, the realm and the password
    /// together; a change to any of them, or to the answer, fails it.
    #[test]
    fn an_answer_authenticates_under_its_own_key_only() {
        let key = key();
        let bytes = message(
            0x0104,
            &[(ATTR_LIFETIME, &600u32.to_be_bytes())],
            Some(&key),
        );
        assert!(response(&bytes).verify(&key));
        for other in [
            Key::long_term("other", REALM, PASS),
            Key::long_term(USER, b"other.example", PASS),
            Key::long_term(USER, REALM, "other"),
        ] {
            assert!(
                !response(&bytes).verify(&other),
                "another key authenticated"
            );
        }

        let mut tampered = bytes.clone();
        tampered[HEADER_LEN + 7] ^= 0x01;
        assert!(
            !response(&tampered).verify(&key),
            "a tampered answer authenticated"
        );

        let bare = message(0x0108, &[], None);
        assert!(!response(&bare).is_authenticated());
        assert!(
            !response(&bare).verify(&key),
            "an answer without integrity verified"
        );
    }

    /// Anything after the integrity attribute is outside what it covers, so
    /// anyone on the path could have appended it. None of it is read.
    #[test]
    fn nothing_after_the_integrity_is_read() {
        let key = key();
        let mut bytes = message(
            0x0103,
            &[(ATTR_LIFETIME, &600u32.to_be_bytes())],
            Some(&key),
        );
        append(&mut bytes, ATTR_LIFETIME, &1u32.to_be_bytes());
        append(
            &mut bytes,
            ATTR_XOR_RELAYED_ADDRESS,
            &xor_address(peer_v4(), TID),
        );

        let answer = response(&bytes);
        assert_eq!(answer.lifetime_s(), Some(600));
        assert_eq!(answer.relayed_address(), None);
        assert!(answer.verify(&key), "what followed the integrity broke it");
    }

    /// Every kind of datagram a relay sends falls on the record side of the
    /// first-two-bytes rule. Only its source can bring it here, which is why the
    /// relay is recognised by address before anything is classified.
    #[test]
    fn nothing_a_relay_sends_is_shaped_like_a_check() {
        let key = key();
        let code = |hundreds: u8, rest: u8| std::vec![0, 0, hundreds, rest];
        let sent = [
            message(0x0103, &[], Some(&key)),
            message(0x0113, &[(ATTR_ERROR_CODE, &code(4, 1))], None),
            message(0x0104, &[], Some(&key)),
            message(0x0114, &[(ATTR_ERROR_CODE, &code(4, 37))], None),
            message(0x0108, &[], Some(&key)),
            message(0x0118, &[(ATTR_ERROR_CODE, &code(4, 38))], None),
            message(0x0109, &[], Some(&key)),
            message(0x0119, &[(ATTR_ERROR_CODE, &code(4, 0))], None),
            message(
                0x0017,
                &[
                    (ATTR_XOR_PEER_ADDRESS, &xor_address(peer_v4(), TID)),
                    (ATTR_DATA, b"x"),
                ],
                None,
            ),
            std::vec![0x40, 0x00, 0x00, 0x01, 0x78],
        ];
        for bytes in &sent {
            assert!(
                parse(bytes).is_ok(),
                "{bytes:02x?} must parse, or this proves nothing"
            );
            assert_eq!(demux::classify(bytes), Datagram::Record, "{bytes:02x?}");
        }
    }

    /// Nothing a relay has no reason to send is read: a request, the client's
    /// own kind of indication, a check's answer, an unknown method, an error
    /// that does not say which, relayed data missing half of itself, integrity
    /// of the wrong size, a foreign cookie, or a length that misses the end.
    #[test]
    fn anything_else_from_the_relay_is_refused() {
        let peer = xor_address(peer_v4(), TID);
        let refusals = [
            message(0x0003, &[], None),
            message(
                0x0016,
                &[(ATTR_XOR_PEER_ADDRESS, &peer), (ATTR_DATA, b"x")],
                None,
            ),
            message(0x0101, &[], None),
            message(0x010A, &[], None),
            message(0x0113, &[], None),
            message(0x0113, &[(ATTR_ERROR_CODE, &[0, 0, 7, 0])], None),
            message(0x0113, &[(ATTR_ERROR_CODE, &[0, 0, 4, 100])], None),
            message(0x0017, &[(ATTR_DATA, b"x")], None),
            message(0x0017, &[(ATTR_XOR_PEER_ADDRESS, &peer)], None),
            message(0x0108, &[(ATTR_MESSAGE_INTEGRITY, &[0; 16])], None),
        ];
        for bytes in &refusals {
            assert_eq!(parse(bytes), Err(Error::Malformed), "{bytes:02x?}");
        }

        let good = message(0x0108, &[], Some(&key()));
        assert!(parse(&good).is_ok());
        let mut foreign = good.clone();
        foreign[4] ^= 0xFF;
        let short = &good[..good.len() - 4];
        let mut long = good.clone();
        long.extend_from_slice(&[0; 4]);
        for bytes in [
            &foreign[..],
            short,
            &long[..],
            &[],
            &[0x80, 0x00, 0x00, 0x00],
        ] {
            assert_eq!(parse(bytes), Err(Error::Malformed), "{bytes:02x?}");
        }
    }
}
