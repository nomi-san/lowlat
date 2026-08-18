//! Record envelope: authenticated encryption around every media datagram.
//!
//! Layout, 29 bytes then ciphertext (docs/01-protocol.md 3):
//!
//! ```text
//! 0   3   magic 17 FE FD
//! 3   8   nonce counter, big endian
//! 11  2   size field, big endian
//! 13  16  authentication tag
//! 29  n   ciphertext
//! ```
//!
//! Two details are easy to get wrong and both are fatal:
//!
//! - **The tag precedes the ciphertext.** It is not appended as in TLS.
//! - **The size field is written and never read.** The plaintext length comes
//!   from the datagram length. Trusting the field is a parsing vulnerability.
//!
//! There is no associated data: the header is not authenticated. So the magic
//! bytes are decoration, and this module does not reject on them. Byte 0 is
//! load bearing only for demultiplexing against connectivity checks, which
//! happens before this module is reached.

use aes_gcm::aead::AeadInPlace;
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit};

use crate::error::{Error, Result};

/// Total envelope overhead.
pub const ENVELOPE_LEN: usize = 29;
/// First byte of the magic. Also what separates a record from a connectivity
/// check during demultiplexing.
pub const MAGIC: [u8; 3] = [0x17, 0xFE, 0xFD];

const NONCE_OFFSET: usize = 3;
const SIZE_OFFSET: usize = 11;
const TAG_OFFSET: usize = 13;
const TAG_LEN: usize = 16;
/// Bytes of the nonce that come from the credential rather than the counter.
const NONCE_PREFIX_LEN: usize = 4;
const CIPHERTEXT_OFFSET: usize = 29;

/// Constant added to the plaintext length when writing the ignored size field.
/// Reproduced so emitted bytes match a peer's exactly; it carries no meaning.
const SIZE_FIELD_BIAS: u16 = 45;

/// Negotiated by credential possession, never on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cipher {
    /// Legacy path, selected when the credential carries no 256-bit key.
    Aes128,
    /// Selected when the credential carries a 256-bit key.
    Aes256,
}

/// The two cipher states differ in size by a quarter kilobyte of expanded
/// key schedule. Boxing is unavailable in a crate with no allocator, and
/// exactly one of these exists per session, built once at setup, so the
/// difference is accepted deliberately.
#[allow(clippy::large_enum_variant)]
enum Keyed {
    Aes128(Aes128Gcm),
    Aes256(Aes256Gcm),
    /// The same two ciphers on the x86-64 AES and carry-less multiply
    /// instructions. Selected at construction when the processor has them,
    /// never by a build flag, because the binary has to run where they are
    /// absent. See `crate::aesni`.
    #[cfg(target_arch = "x86_64")]
    Hw(Cipher, crate::aesni::Aead),
}

/// Seals and opens records for one session.
///
/// Both directions use the same key: the host's. A key offered by the
/// connecting side is a capability signal and is never used to encrypt.
pub struct Envelope {
    keyed: Keyed,
    nonce_prefix: [u8; NONCE_PREFIX_LEN],
}

impl core::fmt::Debug for Envelope {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Never render key material, not even indirectly.
        f.debug_struct("Envelope")
            .field("cipher", &self.cipher())
            .finish_non_exhaustive()
    }
}

impl Cipher {
    /// Key bytes this cipher consumes from the credential.
    pub const fn key_len(self) -> usize {
        match self {
            Cipher::Aes128 => 16,
            Cipher::Aes256 => 32,
        }
    }
}

impl Envelope {
    /// Build from decoded credential material: the key, then a 4-byte nonce
    /// prefix immediately after it.
    ///
    /// **The cipher is not inferred from the length.** The legacy path keys
    /// from the certificate fingerprint, which is 32 bytes of material feeding
    /// a 16-byte key, so a length-based guess would pick the wrong cipher and
    /// fail every packet. Presence of the 256-bit credential decides, and the
    /// caller already knows it.
    pub fn from_credential(material: &[u8], cipher: Cipher) -> Result<Self> {
        let key_len = cipher.key_len();
        let key = material.get(..key_len).ok_or(Error::BadKeyLength)?;
        let prefix: [u8; NONCE_PREFIX_LEN] = material
            .get(key_len..key_len + NONCE_PREFIX_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::BadKeyLength)?;
        Ok(Self::build(key, cipher, prefix))
    }

    /// Build from a bare key with a zero nonce prefix.
    ///
    /// For fixtures and tests. A real session always carries a prefix.
    pub fn from_key(key: &[u8]) -> Result<Self> {
        let cipher = match key.len() {
            16 => Cipher::Aes128,
            32 => Cipher::Aes256,
            _ => return Err(Error::BadKeyLength),
        };
        Ok(Self::build(key, cipher, [0u8; NONCE_PREFIX_LEN]))
    }

    fn build(key: &[u8], cipher: Cipher, nonce_prefix: [u8; NONCE_PREFIX_LEN]) -> Self {
        #[cfg(target_arch = "x86_64")]
        if let Some(aead) = crate::aesni::Aead::new(key) {
            return Self {
                keyed: Keyed::Hw(cipher, aead),
                nonce_prefix,
            };
        }
        let keyed = match cipher {
            Cipher::Aes128 => Keyed::Aes128(Aes128Gcm::new(GenericArray::from_slice(key))),
            Cipher::Aes256 => Keyed::Aes256(Aes256Gcm::new(GenericArray::from_slice(key))),
        };
        Self {
            keyed,
            nonce_prefix,
        }
    }

    /// Whether this session's records go through the hardware implementation.
    ///
    /// The selection is made at construction from what the processor reports,
    /// so a session that quietly fell back to the portable path is worth
    /// seeing in a log rather than inferring from a benchmark. It is also the
    /// only way to test that the dispatch is wired at all.
    pub fn hardware_accelerated(&self) -> bool {
        #[cfg(target_arch = "x86_64")]
        {
            matches!(self.keyed, Keyed::Hw(..))
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            false
        }
    }

    /// Which cipher this session negotiated.
    pub const fn cipher(&self) -> Cipher {
        match self.keyed {
            #[cfg(target_arch = "x86_64")]
            Keyed::Hw(cipher, _) => cipher,
            Keyed::Aes128(_) => Cipher::Aes128,
            Keyed::Aes256(_) => Cipher::Aes256,
        }
    }

    /// Nonce: the session's 4-byte prefix then the counter, big endian.
    ///
    /// Derived, never generated. This is why the core needs no random number
    /// generator and stays deterministic under replay.
    fn nonce_bytes(&self, counter: u64) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[..NONCE_PREFIX_LEN].copy_from_slice(&self.nonce_prefix);
        nonce[NONCE_PREFIX_LEN..].copy_from_slice(&counter.to_be_bytes());
        nonce
    }

    /// Encrypt `plaintext` into `out`, returning the datagram length.
    ///
    /// `out` must hold `plaintext.len() + ENVELOPE_LEN`.
    pub fn seal(&self, counter: u64, plaintext: &[u8], out: &mut [u8]) -> Result<usize> {
        let total = plaintext
            .len()
            .checked_add(ENVELOPE_LEN)
            .ok_or(Error::Oversized)?;
        let body = out
            .get_mut(CIPHERTEXT_OFFSET..total)
            .ok_or(Error::BufferTooSmall)?;
        body.copy_from_slice(plaintext);
        self.seal_in_place(counter, plaintext.len(), out)
    }

    /// Encrypt cleartext that is **already** sitting at `out[ENVELOPE_LEN..]`.
    ///
    /// Lets a caller build a packet directly into its send buffer and wrap it
    /// without a second copy, which is the difference between one and two
    /// passes over every byte on the data path.
    pub fn seal_in_place(
        &self,
        counter: u64,
        plaintext_len: usize,
        out: &mut [u8],
    ) -> Result<usize> {
        let total = plaintext_len
            .checked_add(ENVELOPE_LEN)
            .ok_or(Error::Oversized)?;
        if total > crate::MAX_DATAGRAM {
            return Err(Error::Oversized);
        }
        let out = out.get_mut(..total).ok_or(Error::BufferTooSmall)?;

        let (header, body) = out.split_at_mut(CIPHERTEXT_OFFSET);

        let nonce_bytes = self.nonce_bytes(counter);
        let nonce = GenericArray::from_slice(&nonce_bytes);
        let tag = match &self.keyed {
            #[cfg(target_arch = "x86_64")]
            Keyed::Hw(_, a) => GenericArray::from(a.seal(&nonce_bytes, body)),
            Keyed::Aes128(c) => c
                .encrypt_in_place_detached(nonce, &[], body)
                .map_err(|_| Error::Decrypt)?,
            Keyed::Aes256(c) => c
                .encrypt_in_place_detached(nonce, &[], body)
                .map_err(|_| Error::Decrypt)?,
        };

        // Header is exactly CIPHERTEXT_OFFSET bytes, so every write below is
        // in range by construction.
        let Some(dst) = header.get_mut(..CIPHERTEXT_OFFSET) else {
            return Err(Error::BufferTooSmall);
        };
        let Some(magic) = dst.get_mut(..MAGIC.len()) else {
            return Err(Error::BufferTooSmall);
        };
        magic.copy_from_slice(&MAGIC);
        let Some(n) = dst.get_mut(NONCE_OFFSET..NONCE_OFFSET + 8) else {
            return Err(Error::BufferTooSmall);
        };
        n.copy_from_slice(&counter.to_be_bytes());

        // Reproduced for byte fidelity. A peer never reads it.
        // The ceiling check above bounds the plaintext far below u16::MAX,
        // so the fallback is unreachable; it exists so no cast can truncate.
        let size = u16::try_from(plaintext_len)
            .unwrap_or(u16::MAX)
            .wrapping_add(SIZE_FIELD_BIAS);
        let Some(s) = dst.get_mut(SIZE_OFFSET..SIZE_OFFSET + 2) else {
            return Err(Error::BufferTooSmall);
        };
        s.copy_from_slice(&size.to_be_bytes());

        let Some(t) = dst.get_mut(TAG_OFFSET..TAG_OFFSET + TAG_LEN) else {
            return Err(Error::BufferTooSmall);
        };
        t.copy_from_slice(tag.as_slice());

        Ok(total)
    }

    /// Decrypt `datagram` into `out`, returning the counter and the cleartext.
    ///
    /// The plaintext length is derived from the datagram length. The size field
    /// at offset 11 is deliberately ignored.
    pub fn open<'a>(&self, datagram: &[u8], out: &'a mut [u8]) -> Result<Opened<'a>> {
        if datagram.len() < ENVELOPE_LEN {
            return Err(Error::ShortDatagram);
        }
        let cleartext_len = datagram.len() - ENVELOPE_LEN;

        let counter_bytes: [u8; 8] = datagram
            .get(NONCE_OFFSET..NONCE_OFFSET + 8)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::ShortDatagram)?;
        let counter = u64::from_be_bytes(counter_bytes);

        let tag_bytes: [u8; TAG_LEN] = datagram
            .get(TAG_OFFSET..TAG_OFFSET + TAG_LEN)
            .and_then(|s| s.try_into().ok())
            .ok_or(Error::ShortDatagram)?;

        let ciphertext = datagram
            .get(CIPHERTEXT_OFFSET..)
            .ok_or(Error::ShortDatagram)?;
        let body = out.get_mut(..cleartext_len).ok_or(Error::BufferTooSmall)?;
        body.copy_from_slice(ciphertext);

        let nonce_bytes = self.nonce_bytes(counter);
        let nonce = GenericArray::from_slice(&nonce_bytes);
        let tag = GenericArray::from_slice(&tag_bytes);
        match &self.keyed {
            #[cfg(target_arch = "x86_64")]
            Keyed::Hw(_, a) => {
                if !a.open(&nonce_bytes, body, &tag_bytes) {
                    return Err(Error::Decrypt);
                }
            }
            Keyed::Aes128(c) => c
                .decrypt_in_place_detached(nonce, &[], body, tag)
                .map_err(|_| Error::Decrypt)?,
            Keyed::Aes256(c) => c
                .decrypt_in_place_detached(nonce, &[], body, tag)
                .map_err(|_| Error::Decrypt)?,
        }

        Ok(Opened {
            counter,
            cleartext: body,
        })
    }
}

/// A successfully opened record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened<'a> {
    /// The nonce counter the sender used.
    pub counter: u64,
    /// Verified cleartext. Nothing before this point may be acted on.
    pub cleartext: &'a [u8],
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Breaking the selection in `build` has to fail something. Without this
    /// it did not: both paths produce identical bytes, so every other test
    /// passes either way.
    #[test]
    #[cfg(target_arch = "x86_64")]
    fn the_hardware_path_is_taken_when_the_processor_offers_it() {
        let env = Envelope::from_key(&KEY128).unwrap();
        assert_eq!(env.hardware_accelerated(), crate::aesni::available());
        let env = Envelope::from_key(&KEY256).unwrap();
        assert_eq!(env.hardware_accelerated(), crate::aesni::available());
    }

    const KEY256: [u8; 32] = [7u8; 32];
    const KEY128: [u8; 16] = [9u8; 16];

    #[test]
    fn round_trip_both_ciphers() {
        for key in [KEY128.as_slice(), KEY256.as_slice()] {
            let env = Envelope::from_key(key).unwrap();
            let plaintext = b"the tag precedes the ciphertext";
            let mut wire = [0u8; 256];
            let n = env.seal(42, plaintext, &mut wire).unwrap();
            assert_eq!(n, plaintext.len() + ENVELOPE_LEN);

            let mut out = [0u8; 256];
            let opened = env.open(&wire[..n], &mut out).unwrap();
            assert_eq!(opened.counter, 42);
            assert_eq!(opened.cleartext, plaintext);
        }
    }

    #[test]
    fn layout_is_exact() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut wire = [0u8; 64];
        let n = env.seal(0x0102_0304_0506_0708, b"abcd", &mut wire).unwrap();
        assert_eq!(n, 4 + ENVELOPE_LEN);
        assert_eq!(&wire[0..3], &MAGIC);
        assert_eq!(&wire[3..11], &[1, 2, 3, 4, 5, 6, 7, 8]);
        // 4 + 45 = 49
        assert_eq!(&wire[11..13], &[0, 49]);
    }

    #[test]
    fn a_flipped_byte_fails_authentication() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut wire = [0u8; 128];
        let n = env.seal(1, b"payload", &mut wire).unwrap();
        wire[ENVELOPE_LEN] ^= 0x01;
        let mut out = [0u8; 128];
        assert_eq!(env.open(&wire[..n], &mut out), Err(Error::Decrypt));
    }

    #[test]
    fn a_flipped_tag_fails_authentication() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut wire = [0u8; 128];
        let n = env.seal(1, b"payload", &mut wire).unwrap();
        wire[TAG_OFFSET] ^= 0x80;
        let mut out = [0u8; 128];
        assert_eq!(env.open(&wire[..n], &mut out), Err(Error::Decrypt));
    }

    #[test]
    fn the_wrong_counter_fails_authentication() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut wire = [0u8; 128];
        let n = env.seal(1, b"payload", &mut wire).unwrap();
        wire[NONCE_OFFSET + 7] ^= 0x01;
        let mut out = [0u8; 128];
        assert_eq!(env.open(&wire[..n], &mut out), Err(Error::Decrypt));
    }

    /// The size field is decoration. Corrupting it must change nothing, because
    /// trusting it would be a parsing vulnerability.
    #[test]
    fn the_size_field_is_ignored_on_receive() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut wire = [0u8; 128];
        let n = env.seal(1, b"payload", &mut wire).unwrap();
        wire[SIZE_OFFSET] = 0xFF;
        wire[SIZE_OFFSET + 1] = 0xFF;
        let mut out = [0u8; 128];
        let opened = env.open(&wire[..n], &mut out).unwrap();
        assert_eq!(opened.cleartext, b"payload");
    }

    #[test]
    fn refuses_a_datagram_past_the_ceiling() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let plaintext = [0u8; crate::MAX_CLEARTEXT + 1];
        let mut wire = [0u8; crate::MAX_DATAGRAM + 64];
        assert_eq!(env.seal(0, &plaintext, &mut wire), Err(Error::Oversized));
    }

    #[test]
    fn accepts_a_datagram_exactly_at_the_ceiling() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let plaintext = [0u8; crate::MAX_CLEARTEXT];
        let mut wire = [0u8; crate::MAX_DATAGRAM];
        assert_eq!(
            env.seal(0, &plaintext, &mut wire).unwrap(),
            crate::MAX_DATAGRAM
        );
    }

    #[test]
    fn refuses_bad_key_lengths() {
        assert_eq!(
            Envelope::from_key(&[0u8; 24]).unwrap_err(),
            Error::BadKeyLength
        );
        assert_eq!(Envelope::from_key(&[]).unwrap_err(), Error::BadKeyLength);
    }

    #[test]
    fn refuses_a_short_datagram() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut out = [0u8; 64];
        assert_eq!(
            env.open(&[0u8; ENVELOPE_LEN - 1], &mut out),
            Err(Error::ShortDatagram)
        );
    }

    #[test]
    fn an_empty_cleartext_round_trips() {
        let env = Envelope::from_key(&KEY256).unwrap();
        let mut wire = [0u8; 64];
        let n = env.seal(5, &[], &mut wire).unwrap();
        assert_eq!(n, ENVELOPE_LEN);
        let mut out = [0u8; 64];
        assert_eq!(env.open(&wire[..n], &mut out).unwrap().cleartext, b"");
    }
}
