//! Connectivity check codec: binding requests, responses, and the
//! authentication that makes them trustworthy.
//!
//! Checks share the media socket and are classified before anything else, per
//! docs/01-protocol.md 2. Everything here is pure: bytes in, bytes out, no
//! clock, no socket, no allocation, and no random number generator. Transaction
//! identifiers arrive from the caller for that last reason.
//!
//! Three properties of the wire are load bearing and none of them are obvious.
//!
//! **The length field is written twice.** Integrity is computed over a message
//! whose length claims to end after the integrity attribute; the length left on
//! the wire claims to end after the fingerprint. A verifier that hashes the
//! bytes exactly as received fails every message. Both directions are handled
//! here by feeding the hash a substituted length rather than by copying the
//! message and editing it.
//!
//! **Integrity and fingerprint are adjacent and last.** A peer scans for the
//! pair and rejects anything that separates them, with no diagnostic.
//!
//! **A message outside 52 to 256 bytes is refused before parsing.** Rejecting at
//! the same boundary keeps us from defending a range no peer produces.

use core::net::{IpAddr, Ipv6Addr, SocketAddr};

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::error::{Error, Result};

/// Fixed header: type, length, cookie, transaction identifier.
pub const HEADER_LEN: usize = 20;
/// Constant every message carries, and the seed for address obfuscation.
pub const MAGIC_COOKIE: u32 = 0x2112_A442;
/// Shortest message a peer will look at: header, integrity, fingerprint.
pub const MIN_MESSAGE: usize = 52;
/// Longest message a peer will look at.
pub const MAX_MESSAGE: usize = 256;

/// Largest message this module will build, and so the smallest useful output
/// buffer. A request is header, username, three fixed attributes, integrity,
/// and fingerprint; the username is the only variable part and it is bounded.
pub const MAX_BUILT: usize = MAX_MESSAGE;

/// Longest username fragment accepted on either side.
///
/// Bounded so a username can be assembled without allocating. Well past the
/// four characters real credentials carry.
pub const MAX_UFRAG: usize = 32;

const TYPE_BINDING_REQUEST: u16 = 0x0001;
const TYPE_BINDING_SUCCESS: u16 = 0x0101;

const ATTR_USERNAME: u16 = 0x0006;
const ATTR_MESSAGE_INTEGRITY: u16 = 0x0008;
const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;
const ATTR_PRIORITY: u16 = 0x0024;
const ATTR_FINGERPRINT: u16 = 0x8028;
const ATTR_ICE_CONTROLLED: u16 = 0x8029;
const ATTR_NETWORK_COST: u16 = 0xC057;

/// Integrity attribute: 4 bytes of header, 20 of digest.
const INTEGRITY_LEN: usize = 24;
/// Fingerprint attribute: 4 bytes of header, 4 of checksum.
const FINGERPRINT_LEN: usize = 8;
/// Both together, which is what a message must end with.
const TRAILER_LEN: usize = INTEGRITY_LEN + FINGERPRINT_LEN;

/// Value mixed into the fingerprint so it cannot be confused with a plain
/// checksum of the same bytes.
const FINGERPRINT_XOR: u32 = 0x5354_554E;

/// Advertised path preference. Fixed by the protocol: there is no priority
/// ordering to express, so this never varies.
const PRIORITY_VALUE: u32 = 0x6E00_1EFF;

/// Advertised network identifier and cost. Fixed for the same reason.
const NETWORK_COST_VALUE: u32 = 0x0000_0032;

type HmacSha1 = Hmac<Sha1>;

/// The 96-bit identifier a peer echoes back.
///
/// Derived by the caller from a per-session seed, never generated here. The
/// value only has to be unique among our outstanding transactions: it is echoed
/// rather than validated, and the integrity attribute is what authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionId(pub [u8; 12]);

/// What a well-formed message turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Method {
    /// A peer is checking whether we are reachable. Answer it.
    BindingRequest,
    /// A peer answered our check.
    BindingSuccess,
}

/// A parsed message, borrowing the datagram it came from.
#[derive(Debug)]
pub struct Message<'a> {
    bytes: &'a [u8],
    method: Method,
    tid: TransactionId,
    /// Offset of the integrity attribute, which is where the hash stops.
    integrity_at: usize,
}

impl<'a> Message<'a> {
    /// Classify and structurally validate a datagram.
    ///
    /// Accepts only what a peer would accept, and does not authenticate.
    /// [`Message::verify`] is a separate step because a caller answering a
    /// check and a caller matching a response use different passwords.
    pub fn parse(datagram: &'a [u8]) -> Result<Self> {
        if datagram.len() < MIN_MESSAGE || datagram.len() > MAX_MESSAGE {
            return Err(Error::Malformed);
        }
        let head = datagram.get(..HEADER_LEN).ok_or(Error::Malformed)?;

        let kind = be16(head, 0)?;
        let method = match kind {
            TYPE_BINDING_REQUEST => Method::BindingRequest,
            TYPE_BINDING_SUCCESS => Method::BindingSuccess,
            _ => return Err(Error::Malformed),
        };

        if be32(head, 4)? != MAGIC_COOKIE {
            return Err(Error::Malformed);
        }

        // The length field counts everything after the header and must land
        // exactly on the end of the datagram; a peer that trusted a short value
        // would authenticate a prefix and act on the rest.
        let claimed = usize::from(be16(head, 2)?);
        if claimed != datagram.len() - HEADER_LEN {
            return Err(Error::Malformed);
        }

        let mut tid = [0u8; 12];
        tid.copy_from_slice(head.get(8..20).ok_or(Error::Malformed)?);

        // Integrity and fingerprint are required, adjacent, and last, so their
        // offsets follow from the length alone rather than from a scan.
        let integrity_at = datagram.len() - TRAILER_LEN;
        if be16(datagram, integrity_at)? != ATTR_MESSAGE_INTEGRITY
            || be16(datagram, integrity_at + 2)? != 20
            || be16(datagram, integrity_at + INTEGRITY_LEN)? != ATTR_FINGERPRINT
            || be16(datagram, integrity_at + INTEGRITY_LEN + 2)? != 4
        {
            return Err(Error::Malformed);
        }

        // Walking the attributes proves the trailer is where the encoding says
        // it is, rather than merely where the length puts it.
        let mut walk = Attributes::new(datagram, integrity_at);
        let mut ok = false;
        while let Some(attribute) = walk.next() {
            let (_, _, end) = attribute?;
            ok = end == integrity_at;
        }
        if !ok && integrity_at != HEADER_LEN {
            return Err(Error::Malformed);
        }

        Ok(Self {
            bytes: datagram,
            method,
            tid: TransactionId(tid),
            integrity_at,
        })
    }

    /// What kind of message this is.
    pub fn method(&self) -> Method {
        self.method
    }

    /// The identifier to match against an outstanding transaction.
    pub fn transaction_id(&self) -> TransactionId {
        self.tid
    }

    /// True if the message authenticates under `password`.
    ///
    /// The fingerprint is checked first because it is cheap and catches a
    /// truncated or spliced message before a digest is computed over it.
    pub fn verify(&self, password: &str) -> bool {
        let Ok(expected) = be32(self.bytes, self.integrity_at + INTEGRITY_LEN + 4) else {
            return false;
        };
        let Ok(actual) = fingerprint_of(self.bytes, self.integrity_at) else {
            return false;
        };
        if expected != actual {
            return false;
        }

        let Ok(mac) = integrity_of(self.bytes, self.integrity_at, password) else {
            return false;
        };
        let Some(carried) = self
            .bytes
            .get(self.integrity_at + 4..self.integrity_at + 24)
        else {
            return false;
        };
        // Constant-time: a forged message must not be distinguishable by how
        // long the comparison took.
        let mut diff = 0u8;
        for (a, b) in mac.iter().zip(carried.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// The username this message carries, if any.
    pub fn username(&self) -> Option<&'a str> {
        let mut walk = Attributes::new(self.bytes, self.integrity_at);
        while let Some(Ok((kind, value, _))) = walk.next() {
            if kind == ATTR_USERNAME {
                return core::str::from_utf8(value).ok();
            }
        }
        None
    }

    /// The reflexive address a peer observed for us, decoded.
    pub fn mapped_address(&self) -> Option<SocketAddr> {
        let mut walk = Attributes::new(self.bytes, self.integrity_at);
        while let Some(Ok((kind, value, _))) = walk.next() {
            if kind == ATTR_XOR_MAPPED_ADDRESS {
                return decode_mapped(value, self.tid);
            }
        }
        None
    }
}

/// Build an authenticated binding request into `out`.
///
/// `password` is the peer's, from the credential exchange. The username is
/// their fragment, a colon, then ours, which is the order a peer matches on.
pub fn encode_binding_request(
    out: &mut [u8],
    tid: TransactionId,
    local_ufrag: &str,
    remote_ufrag: &str,
    tiebreaker: [u8; 8],
    password: &str,
) -> Result<usize> {
    if local_ufrag.len() > MAX_UFRAG || remote_ufrag.len() > MAX_UFRAG {
        return Err(Error::Oversized);
    }

    let mut w = Writer::new(out);
    w.header(TYPE_BINDING_REQUEST, tid)?;

    // Username is written in pieces so the pair never needs a scratch buffer.
    let user_len = remote_ufrag
        .len()
        .checked_add(1)
        .and_then(|n| n.checked_add(local_ufrag.len()))
        .ok_or(Error::Oversized)?;
    w.attribute_header(ATTR_USERNAME, user_len)?;
    w.put(remote_ufrag.as_bytes())?;
    w.put(b":")?;
    w.put(local_ufrag.as_bytes())?;
    w.pad(user_len)?;

    w.attribute(ATTR_NETWORK_COST, &NETWORK_COST_VALUE.to_be_bytes())?;
    w.attribute(ATTR_ICE_CONTROLLED, &tiebreaker)?;
    w.attribute(ATTR_PRIORITY, &PRIORITY_VALUE.to_be_bytes())?;

    w.seal(password)
}

/// Build an authenticated binding response into `out`.
///
/// `password` is ours. `observed` is where the request actually came from,
/// which is the whole point of answering.
pub fn encode_binding_response(
    out: &mut [u8],
    tid: TransactionId,
    observed: SocketAddr,
    password: &str,
) -> Result<usize> {
    let mut w = Writer::new(out);
    w.header(TYPE_BINDING_SUCCESS, tid)?;

    let mut value = [0u8; 20];
    let written = encode_mapped(&mut value, observed, tid)?;
    w.attribute(
        ATTR_XOR_MAPPED_ADDRESS,
        value.get(..written).ok_or(Error::BufferTooSmall)?,
    )?;

    w.seal(password)
}

/// Collapse a v4-mapped address to the IPv4 it actually is.
///
/// Structural, never textual. A v4-mapped address contains colons in its text
/// form, so classifying by searching for one removes every IPv4 candidate and
/// kills connectivity on v4-only paths.
pub fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}

/// Writes a message, tracking where the trailer will go.
struct Writer<'a> {
    buf: &'a mut [u8],
    at: usize,
    tid: TransactionId,
}

impl<'a> Writer<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            at: 0,
            tid: TransactionId([0u8; 12]),
        }
    }

    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.at.checked_add(bytes.len()).ok_or(Error::Oversized)?;
        let slot = self
            .buf
            .get_mut(self.at..end)
            .ok_or(Error::BufferTooSmall)?;
        slot.copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }

    fn header(&mut self, kind: u16, tid: TransactionId) -> Result<()> {
        self.tid = tid;
        self.put(&kind.to_be_bytes())?;
        // Length is a placeholder until seal knows where the trailer lands.
        self.put(&0u16.to_be_bytes())?;
        self.put(&MAGIC_COOKIE.to_be_bytes())?;
        self.put(&tid.0)
    }

    fn attribute_header(&mut self, kind: u16, len: usize) -> Result<()> {
        let len = u16::try_from(len).map_err(|_| Error::Oversized)?;
        self.put(&kind.to_be_bytes())?;
        self.put(&len.to_be_bytes())
    }

    /// Pad a value out to a four-byte boundary.
    fn pad(&mut self, len: usize) -> Result<()> {
        let pad = (4 - len % 4) % 4;
        let zeros = [0u8; 3];
        self.put(zeros.get(..pad).ok_or(Error::Malformed)?)
    }

    fn attribute(&mut self, kind: u16, value: &[u8]) -> Result<()> {
        self.attribute_header(kind, value.len())?;
        self.put(value)?;
        self.pad(value.len())
    }

    /// Append integrity and fingerprint, fix the length, and finish.
    fn seal(self, password: &str) -> Result<usize> {
        let at = self.at;
        let total = at.checked_add(TRAILER_LEN).ok_or(Error::Oversized)?;
        if total > MAX_MESSAGE {
            return Err(Error::Oversized);
        }
        if total < MIN_MESSAGE {
            return Err(Error::Malformed);
        }
        let buf = self.buf.get_mut(..total).ok_or(Error::BufferTooSmall)?;

        put_be16(buf, at, ATTR_MESSAGE_INTEGRITY)?;
        put_be16(buf, at + 2, 20)?;
        let mac = integrity_of(buf, at, password)?;
        buf.get_mut(at + 4..at + 24)
            .ok_or(Error::BufferTooSmall)?
            .copy_from_slice(&mac);

        put_be16(buf, at + INTEGRITY_LEN, ATTR_FINGERPRINT)?;
        put_be16(buf, at + INTEGRITY_LEN + 2, 4)?;
        let crc = fingerprint_of(buf, at)?;
        buf.get_mut(at + INTEGRITY_LEN + 4..at + TRAILER_LEN)
            .ok_or(Error::BufferTooSmall)?
            .copy_from_slice(&crc.to_be_bytes());

        // The length on the wire is the fingerprint-inclusive one. It is
        // written last because both hashes above needed a different value.
        put_be16(
            buf,
            2,
            u16::try_from(at + 12).map_err(|_| Error::Oversized)?,
        )?;
        Ok(total)
    }
}

/// Iterates attributes between the header and the trailer.
struct Attributes<'a> {
    bytes: &'a [u8],
    at: usize,
    end: usize,
    done: bool,
}

impl<'a> Attributes<'a> {
    fn new(bytes: &'a [u8], end: usize) -> Self {
        Self {
            bytes,
            at: HEADER_LEN,
            end,
            done: false,
        }
    }

    /// Next attribute as type, value, and the offset just past its padding.
    #[allow(clippy::should_implement_trait)]
    fn next(&mut self) -> Option<Result<(u16, &'a [u8], usize)>> {
        if self.done || self.at + 4 > self.end {
            return None;
        }
        let kind = match be16(self.bytes, self.at) {
            Ok(kind) => kind,
            Err(error) => {
                self.done = true;
                return Some(Err(error));
            }
        };
        let len = match be16(self.bytes, self.at + 2) {
            Ok(len) => usize::from(len),
            Err(error) => {
                self.done = true;
                return Some(Err(error));
            }
        };
        let start = self.at + 4;
        let stop = match start.checked_add(len) {
            Some(stop) if stop <= self.end => stop,
            _ => {
                self.done = true;
                return Some(Err(Error::Malformed));
            }
        };
        let Some(value) = self.bytes.get(start..stop) else {
            self.done = true;
            return Some(Err(Error::Malformed));
        };
        let next = stop + (4 - len % 4) % 4;
        self.at = next;
        Some(Ok((kind, value, next)))
    }
}

/// HMAC-SHA1 over the message up to `at`, with the length field substituted.
///
/// The substituted value claims the message ends after the integrity
/// attribute, which is what both sides agree to hash. Feeding the digest in
/// three pieces avoids copying the message to edit two bytes.
fn integrity_of(bytes: &[u8], at: usize, password: &str) -> Result<[u8; 20]> {
    let mut mac = HmacSha1::new_from_slice(password.as_bytes()).map_err(|_| Error::BadKeyLength)?;
    let claimed = u16::try_from(at + 4).map_err(|_| Error::Oversized)?;
    mac.update(bytes.get(..2).ok_or(Error::Malformed)?);
    mac.update(&claimed.to_be_bytes());
    mac.update(bytes.get(4..at).ok_or(Error::Malformed)?);
    Ok(mac.finalize().into_bytes().into())
}

/// Fingerprint over the message through the integrity attribute, with the
/// length field substituted for the fingerprint-inclusive value.
fn fingerprint_of(bytes: &[u8], at: usize) -> Result<u32> {
    let claimed = u16::try_from(at + 12).map_err(|_| Error::Oversized)?;
    let mut crc = Crc32::new();
    crc.update(bytes.get(..2).ok_or(Error::Malformed)?);
    crc.update(&claimed.to_be_bytes());
    crc.update(bytes.get(4..at + INTEGRITY_LEN).ok_or(Error::Malformed)?);
    Ok(crc.finish() ^ FINGERPRINT_XOR)
}

/// Obfuscate an address into an attribute value, returning its length.
fn encode_mapped(out: &mut [u8], addr: SocketAddr, tid: TransactionId) -> Result<usize> {
    let addr = canonical(addr);
    let cookie = MAGIC_COOKIE.to_be_bytes();
    let port = addr.port() ^ 0x2112;

    out.get_mut(0..1).ok_or(Error::BufferTooSmall)?.fill(0);
    put_be16(out, 2, port)?;

    match addr.ip() {
        IpAddr::V4(v4) => {
            *out.get_mut(1).ok_or(Error::BufferTooSmall)? = 0x01;
            let slot = out.get_mut(4..8).ok_or(Error::BufferTooSmall)?;
            for (index, byte) in v4.octets().iter().enumerate() {
                *slot.get_mut(index).ok_or(Error::BufferTooSmall)? =
                    byte ^ cookie.get(index).copied().unwrap_or(0);
            }
            Ok(8)
        }
        IpAddr::V6(v6) => {
            *out.get_mut(1).ok_or(Error::BufferTooSmall)? = 0x02;
            let slot = out.get_mut(4..20).ok_or(Error::BufferTooSmall)?;
            for (index, byte) in v6.octets().iter().enumerate() {
                let mask = match cookie.get(index) {
                    Some(byte) => *byte,
                    None => tid.0.get(index - 4).copied().unwrap_or(0),
                };
                *slot.get_mut(index).ok_or(Error::BufferTooSmall)? = byte ^ mask;
            }
            Ok(20)
        }
    }
}

/// Decode an obfuscated address back into a socket address.
fn decode_mapped(value: &[u8], tid: TransactionId) -> Option<SocketAddr> {
    let family = *value.get(1)?;
    let port = u16::from_be_bytes([*value.get(2)?, *value.get(3)?]) ^ 0x2112;
    let cookie = MAGIC_COOKIE.to_be_bytes();

    match family {
        0x01 => {
            let raw = value.get(4..8)?;
            let mut octets = [0u8; 4];
            for (index, slot) in octets.iter_mut().enumerate() {
                *slot = raw.get(index)? ^ cookie.get(index)?;
            }
            Some(SocketAddr::new(IpAddr::V4(octets.into()), port))
        }
        0x02 => {
            let raw = value.get(4..20)?;
            let mut octets = [0u8; 16];
            for (index, slot) in octets.iter_mut().enumerate() {
                let mask = match cookie.get(index) {
                    Some(byte) => *byte,
                    None => *tid.0.get(index - 4)?,
                };
                *slot = raw.get(index)? ^ mask;
            }
            Some(canonical(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::from(octets)),
                port,
            )))
        }
        _ => None,
    }
}

/// CRC-32, the reflected polynomial, computed a byte at a time.
///
/// Not a cryptographic primitive and not on a hot path: checks are emitted a
/// few times a second. A table would buy nothing here and costs a kilobyte.
struct Crc32(u32);

impl Crc32 {
    fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = (self.0 & 1).wrapping_neg();
                self.0 = (self.0 >> 1) ^ (0xEDB8_8320 & mask);
            }
        }
    }

    fn finish(self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

fn be16(bytes: &[u8], at: usize) -> Result<u16> {
    let slot = bytes.get(at..at + 2).ok_or(Error::Malformed)?;
    Ok(u16::from_be_bytes([
        *slot.first().ok_or(Error::Malformed)?,
        *slot.get(1).ok_or(Error::Malformed)?,
    ]))
}

fn be32(bytes: &[u8], at: usize) -> Result<u32> {
    let slot = bytes.get(at..at + 4).ok_or(Error::Malformed)?;
    let mut value = [0u8; 4];
    value.copy_from_slice(slot);
    Ok(u32::from_be_bytes(value))
}

fn put_be16(bytes: &mut [u8], at: usize, value: u16) -> Result<()> {
    bytes
        .get_mut(at..at + 2)
        .ok_or(Error::BufferTooSmall)?
        .copy_from_slice(&value.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::net::{Ipv4Addr, SocketAddrV6};
    use std::string::ToString;

    const TID: TransactionId = TransactionId([
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
    ]);
    const TIEBREAK: [u8; 8] = [0xAA; 8];
    const PWD: &str = "thepassword";

    fn request(out: &mut [u8]) -> usize {
        encode_binding_request(out, TID, "loca", "remo", TIEBREAK, PWD).unwrap()
    }

    #[test]
    fn crc32_matches_the_standard_vector() {
        let mut crc = Crc32::new();
        crc.update(b"123456789");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }

    #[test]
    fn a_request_round_trips_and_authenticates() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);

        let message = Message::parse(&buf[..len]).unwrap();
        assert_eq!(message.method(), Method::BindingRequest);
        assert_eq!(message.transaction_id(), TID);
        assert_eq!(message.username(), Some("remo:loca"));
        assert!(message.verify(PWD));
        assert!(!message.verify("wrong"), "a bad password authenticated");
    }

    /// The length field carried on the wire is the fingerprint-inclusive value,
    /// while the integrity digest is taken over the shorter one. A verifier
    /// that hashes the bytes as received computes a different digest and
    /// rejects every message, which is the failure this substitution avoids.
    #[test]
    fn integrity_is_taken_over_a_substituted_length() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);
        let at = len - TRAILER_LEN;

        let on_the_wire = be16(&buf, 2).unwrap();
        assert_eq!(usize::from(on_the_wire), at + 12);

        let mut naive = HmacSha1::new_from_slice(PWD.as_bytes()).unwrap();
        naive.update(&buf[..at]);
        let naive: [u8; 20] = naive.finalize().into_bytes().into();

        assert_eq!(
            &integrity_of(&buf[..len], at, PWD).unwrap()[..],
            &buf[at + 4..at + 24],
            "the digest on the wire is the one taken over the substituted length"
        );
        assert_ne!(
            &naive[..],
            &buf[at + 4..at + 24],
            "hashing the bytes as received agreed, so the substitution is not load bearing \
             and this test proves nothing"
        );
    }

    #[test]
    fn integrity_and_fingerprint_are_adjacent_and_last() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);
        let at = len - TRAILER_LEN;

        assert_eq!(be16(&buf, at).unwrap(), ATTR_MESSAGE_INTEGRITY);
        assert_eq!(be16(&buf, at + 2).unwrap(), 20);
        assert_eq!(be16(&buf, at + INTEGRITY_LEN).unwrap(), ATTR_FINGERPRINT);
        assert_eq!(be16(&buf, at + INTEGRITY_LEN + 2).unwrap(), 4);
        assert_eq!(at + TRAILER_LEN, len);
    }

    #[test]
    fn a_tampered_message_is_refused() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);

        // Flip a byte inside the username, leaving the structure intact.
        buf[HEADER_LEN + 4] ^= 0xFF;
        let message = Message::parse(&buf[..len]).unwrap();
        assert!(!message.verify(PWD));
    }

    #[test]
    fn a_response_carries_the_observed_address() {
        for observed in [
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 51_234),
            SocketAddr::new(
                IpAddr::V6("2001:db8::1".parse::<Ipv6Addr>().unwrap()),
                4_242,
            ),
        ] {
            let mut buf = [0u8; MAX_BUILT];
            let len = encode_binding_response(&mut buf, TID, observed, PWD).unwrap();

            let message = Message::parse(&buf[..len]).unwrap();
            assert_eq!(message.method(), Method::BindingSuccess);
            assert!(message.verify(PWD));
            assert_eq!(message.mapped_address(), Some(observed));
        }
    }

    /// A v4-mapped address contains colons in its text form and is IPv4.
    /// Classifying it by looking for one removes every v4 candidate and kills
    /// connectivity on v4-only paths.
    #[test]
    fn a_v4_mapped_address_is_classified_as_v4() {
        let mapped = Ipv6Addr::from([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xFF, 198, 51, 100, 9]);
        let addr = SocketAddr::V6(SocketAddrV6::new(mapped, 9_000, 0, 0));

        assert!(
            mapped.to_string().contains(':'),
            "the text form must contain a colon, or this proves nothing"
        );
        assert_eq!(
            canonical(addr),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)), 9_000)
        );

        // And it survives a round trip as an IPv4 mapped address.
        let mut buf = [0u8; MAX_BUILT];
        let len = encode_binding_response(&mut buf, TID, addr, PWD).unwrap();
        let message = Message::parse(&buf[..len]).unwrap();
        assert_eq!(
            message.mapped_address(),
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)),
                9_000
            ))
        );
    }

    #[test]
    fn messages_outside_the_accepted_window_are_refused() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);

        assert_eq!(
            Message::parse(&buf[..MIN_MESSAGE - 1]).unwrap_err(),
            Error::Malformed
        );
        assert!(Message::parse(&buf[..len]).is_ok());

        // A username long enough to push past the ceiling is refused outright.
        let long = "u".repeat(MAX_UFRAG + 1);
        assert_eq!(
            encode_binding_request(&mut buf, TID, &long, "remo", TIEBREAK, PWD),
            Err(Error::Oversized)
        );
    }

    #[test]
    fn a_truncated_or_extended_message_is_refused() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);

        // The length field must land exactly on the end of the datagram.
        let mut short = buf;
        put_be16(&mut short, 2, u16::try_from(len - HEADER_LEN - 1).unwrap()).unwrap();
        assert_eq!(Message::parse(&short[..len]).unwrap_err(), Error::Malformed);
    }

    #[test]
    fn a_foreign_cookie_is_refused() {
        let mut buf = [0u8; MAX_BUILT];
        let len = request(&mut buf);
        buf[4] ^= 0xFF;
        assert_eq!(Message::parse(&buf[..len]).unwrap_err(), Error::Malformed);
    }
}
