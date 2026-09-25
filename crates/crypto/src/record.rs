//! The record cipher: a session's AES-GCM on a vetted implementation, lent to
//! the core's envelope.
//!
//! The core seals with a portable implementation of its own, which stays the
//! reference this one is tested against byte for byte. This one takes the
//! processor's AES and carry-less multiply instructions, their wide forms
//! where they exist, and constant-time software where they do not, choosing
//! at run time. The core cannot own it: the library carries a random source,
//! and the core must be unable to reach one (docs/00-overview.md D4).

use lowlat_core::envelope::{Aead, Cipher, NONCE_LEN, TAG_LEN};
use ring::aead::{AES_128_GCM, AES_256_GCM, Aad, LessSafeKey, Nonce, Tag, UnboundKey};

use crate::Error;

/// One session's key, kept by the thread that runs the session and lent to
/// its envelope for the session's life.
pub struct Record(LessSafeKey);

impl Record {
    /// Key the cipher from decoded credential material, read the way the
    /// envelope reads it ([`Cipher::key`]).
    pub fn new(material: &[u8], cipher: Cipher) -> Result<Self, Error> {
        let key = cipher.key(material).map_err(|_| Error::Material)?;
        let algorithm = match cipher {
            Cipher::Aes128 => &AES_128_GCM,
            Cipher::Aes256 => &AES_256_GCM,
        };
        let key = UnboundKey::new(algorithm, key).map_err(|_| Error::Material)?;
        Ok(Self(LessSafeKey::new(key)))
    }
}

/// Never renders the key, not even its algorithm's internals.
impl core::fmt::Debug for Record {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Record(..)")
    }
}

impl Aead for Record {
    fn seal(&self, nonce: &[u8; NONCE_LEN], body: &mut [u8]) -> lowlat_core::Result<[u8; TAG_LEN]> {
        let tag = self
            .0
            .seal_in_place_separate_tag(Nonce::assume_unique_for_key(*nonce), Aad::empty(), body)
            .map_err(|_| lowlat_core::Error::Decrypt)?;
        <[u8; TAG_LEN]>::try_from(tag.as_ref()).map_err(|_| lowlat_core::Error::Decrypt)
    }

    fn open(
        &self,
        nonce: &[u8; NONCE_LEN],
        body: &mut [u8],
        tag: &[u8; TAG_LEN],
    ) -> lowlat_core::Result<()> {
        self.0
            .open_in_place_separate_tag(
                Nonce::assume_unique_for_key(*nonce),
                Aad::empty(),
                Tag::from(*tag),
                body,
                0..,
            )
            .map(|_| ())
            .map_err(|_| lowlat_core::Error::Decrypt)
    }
}

#[cfg(test)]
// Fixtures fill buffers from loop counters, where the wrap is the point.
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;
    use lowlat_core::envelope::{ENVELOPE_LEN, Envelope};

    /// Key then prefix, as the credential decodes, for either cipher.
    fn material(cipher: Cipher) -> Vec<u8> {
        let len = cipher.key_len() + 4;
        (0..len).map(|i| (i * 37 + 11) as u8).collect()
    }

    /// Every length a group-at-a-time implementation treats differently:
    /// empty, each tail inside the first groups, both sides of a group
    /// boundary out to sixteen blocks, then the datagrams the stream sends.
    fn lengths() -> Vec<usize> {
        let mut out: Vec<usize> = (0..=300).collect();
        out.extend([511, 512, 513, 1023, 1024, 1025, 1200]);
        out.push(lowlat_core::MAX_CLEARTEXT);
        out
    }

    /// The property that matters: a peer cannot tell which implementation
    /// sealed a record. Same bytes out, and each opens the other's.
    #[test]
    fn it_writes_the_records_the_portable_cipher_writes() {
        for cipher in [Cipher::Aes128, Cipher::Aes256] {
            let material = material(cipher);
            let record = Record::new(&material, cipher).unwrap();
            let lent = Envelope::lent(&record, &material, cipher).unwrap();
            let own = Envelope::from_credential(&material, cipher).unwrap();
            let mut a = vec![0u8; lowlat_core::MAX_DATAGRAM];
            let mut b = vec![0u8; lowlat_core::MAX_DATAGRAM];
            let mut out = vec![0u8; lowlat_core::MAX_DATAGRAM];
            for len in lengths() {
                let plaintext: Vec<u8> = (0..len).map(|i| (i * 7 + len) as u8).collect();
                let counter = 1000 + len as u64;
                let n = lent.seal(counter, &plaintext, &mut a).unwrap();
                assert_eq!(n, len + ENVELOPE_LEN);
                assert_eq!(own.seal(counter, &plaintext, &mut b).unwrap(), n);
                assert!(
                    a[..n] == b[..n],
                    "{cipher:?} {len} bytes: the records differ"
                );
                let opened = own.open(&a[..n], &mut out).unwrap();
                assert_eq!(opened.cleartext, &plaintext[..], "{cipher:?} {len}");
                let opened = lent.open(&b[..n], &mut out).unwrap();
                assert_eq!(opened.cleartext, &plaintext[..], "{cipher:?} {len}");
            }
        }
    }

    #[test]
    fn a_record_touched_anywhere_is_refused() {
        for cipher in [Cipher::Aes128, Cipher::Aes256] {
            let material = material(cipher);
            let record = Record::new(&material, cipher).unwrap();
            let lent = Envelope::lent(&record, &material, cipher).unwrap();
            let mut wire = [0u8; 256];
            let n = lent.seal(5, &[0x5A; 200], &mut wire).unwrap();
            let mut out = [0u8; 256];
            assert!(lent.open(&wire[..n], &mut out).is_ok());
            // The counter, the tag, the first and the last byte of the body.
            for at in [10, 13, 28, ENVELOPE_LEN, n - 1] {
                let mut touched = wire;
                touched[at] ^= 0x01;
                assert!(
                    lent.open(&touched[..n], &mut out).is_err(),
                    "{cipher:?}: a flip at byte {at} opened"
                );
            }
        }
    }

    #[test]
    fn material_too_short_for_the_cipher_is_refused() {
        let short = material(Cipher::Aes128);
        assert_eq!(
            Record::new(&short[..16], Cipher::Aes256).unwrap_err(),
            Error::Material
        );
        assert!(Record::new(&short, Cipher::Aes128).is_ok());
    }

    #[test]
    fn the_key_does_not_reach_the_debug_output() {
        let material = material(Cipher::Aes256);
        let record = Record::new(&material, Cipher::Aes256).unwrap();
        assert_eq!(format!("{record:?}"), "Record(..)");
    }
}
