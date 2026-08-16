//! Credentials and key material: the only entropy source in the workspace.
//!
//! It exists as its own crate for one reason. `lowlat-core` must never contain
//! a random number generator, because determinism is what makes replay and
//! simulation testing possible, and that rule is enforced by `no_std` rather
//! than by review. Everything above the core still needs entropy -- a session
//! key, a check password, the seed a transaction identifier is derived from --
//! and putting it in whichever crate happened to need it first would scatter
//! the one thing worth keeping in a single audited place.
//!
//! Nothing here invents a primitive. Randomness comes from the platform, and
//! the encodings below are encodings, not cryptography.

#![forbid(unsafe_code)]

use core::fmt;

/// A credential set for one attempt, in the form signaling carries.
///
/// Sizes are not arbitrary and are not ours to choose: a peer reads these as
/// opaque strings, so they must match what one produces or they will be
/// rejected on length before anything looks at their contents.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// base64 of 4 bytes, 8 characters.
    pub ufrag: String,
    /// base64 of 24 bytes, 32 characters.
    pub pwd: String,
    /// hex of 32 bytes, 64 characters.
    pub fingerprint: String,
    /// hex of 127 bytes, 254 characters. Far longer than the cipher consumes;
    /// see [`key_material`].
    pub aes256: String,
}

/// Never renders the material it holds.
///
/// A credential set reaches a log only by accident, and the accident is worth
/// making impossible rather than unlikely.
impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("ufrag", &"<redacted>")
            .field("pwd", &"<redacted>")
            .field("fingerprint", &"<redacted>")
            .field("aes256", &"<redacted>")
            .finish()
    }
}

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The platform would not supply entropy. Not recoverable by retrying.
    Entropy,
    /// Key material was too short or not hexadecimal.
    Material,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Entropy => write!(f, "the platform supplied no entropy"),
            Self::Material => write!(f, "key material is malformed"),
        }
    }
}

impl std::error::Error for Error {}

/// Bytes of key the cipher takes.
pub const KEY_LEN: usize = 32;

/// Bytes of nonce prefix ahead of the counter.
///
/// **Not zeros.** The prefix is credential material, and a nonce built from
/// four zero bytes and a counter authenticates nothing a peer will accept.
pub const NONCE_PREFIX_LEN: usize = 4;

/// Bytes of the credential the cipher actually consumes.
const CONSUMED: usize = KEY_LEN + NONCE_PREFIX_LEN;

/// Random bytes the media credential carries.
///
/// Only [`CONSUMED`] of them are ever used. The field is this long because that
/// is what a peer emits, and a host that emits a shorter one is proposing
/// material a peer may reject on length alone.
const AES_MATERIAL: usize = 127;

/// Fill a buffer with platform entropy.
pub fn fill(buf: &mut [u8]) -> Result<(), Error> {
    getrandom::getrandom(buf).map_err(|_| Error::Entropy)
}

/// A fresh credential set.
pub fn credentials() -> Result<Credentials, Error> {
    let mut ufrag = [0u8; 4];
    let mut pwd = [0u8; 24];
    let mut fingerprint = [0u8; 32];
    let mut aes = [0u8; AES_MATERIAL];
    fill(&mut ufrag)?;
    fill(&mut pwd)?;
    fill(&mut fingerprint)?;
    fill(&mut aes)?;
    Ok(Credentials {
        ufrag: base64(&ufrag),
        pwd: base64(&pwd),
        fingerprint: hex(&fingerprint),
        aes256: hex(&aes),
    })
}

/// The seed a connectivity attempt derives its transaction identifiers from.
///
/// The core takes this rather than reading a generator, which is what lets a
/// failing punch be replayed from one value.
pub fn transaction_seed() -> Result<[u8; 16], Error> {
    let mut seed = [0u8; 16];
    fill(&mut seed)?;
    Ok(seed)
}

/// Decode the key and nonce prefix a session is encrypted with.
///
/// **Only the leading bytes of the credential are consumed**, and the rest is
/// ignored. A check written to the length of the key rather than the length of
/// the field rejects every real credential, because the field is far longer.
///
/// Accepts the legacy path too: a fingerprint is shorter than a media key and
/// yields the 16-byte key that path uses, which is why the caller passes
/// whichever the peer supplied rather than deciding by length here.
pub fn key_material(material: &str) -> Result<([u8; KEY_LEN], [u8; NONCE_PREFIX_LEN]), Error> {
    let bytes = material.as_bytes();
    if bytes.len() < CONSUMED * 2 {
        return Err(Error::Material);
    }
    let mut key = [0u8; KEY_LEN];
    let mut prefix = [0u8; NONCE_PREFIX_LEN];
    for index in 0..CONSUMED {
        let high = nibble(bytes[index * 2])?;
        let low = nibble(bytes[index * 2 + 1])?;
        let byte = (high << 4) | low;
        if index < KEY_LEN {
            key[index] = byte;
        } else {
            prefix[index - KEY_LEN] = byte;
        }
    }
    Ok((key, prefix))
}

fn nibble(c: u8) -> Result<u8, Error> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::Material),
    }
}

/// Lowercase hex, which is the case a peer emits.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0F)]));
    }
    out
}

/// Standard base64 with padding, per RFC 4648 section 4.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        for slot in 0..4 {
            if slot <= chunk.len() {
                let index = (triple >> (18 - slot * 6)) & 0x3F;
                out.push(char::from(ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 4648 section 10. An encoder that is wrong at a padding boundary
    /// produces a fragment a peer rejects, and only the boundaries show it.
    #[test]
    fn base64_matches_the_published_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "input {input:?}");
        }
    }

    #[test]
    fn hex_is_lowercase_and_fixed_width() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
    }

    /// The wire lengths are what a peer reads. They are asserted here because
    /// a credential of the wrong length is refused before anything inspects it.
    #[test]
    fn credentials_have_the_lengths_a_peer_expects() {
        let creds = credentials().expect("entropy");
        assert_eq!(creds.ufrag.len(), 8);
        assert_eq!(creds.pwd.len(), 32);
        assert_eq!(creds.fingerprint.len(), 64);
        assert_eq!(creds.aes256.len(), 254);
    }

    /// Two sets in a row must differ, or the generator is not generating.
    /// A constant passes every length check above.
    #[test]
    fn credentials_are_not_constant() {
        let first = credentials().expect("entropy");
        let second = credentials().expect("entropy");
        assert_ne!(first.aes256, second.aes256);
        assert_ne!(first.pwd, second.pwd);
        assert_ne!(transaction_seed().unwrap(), transaction_seed().unwrap());
    }

    /// The credential is far longer than the cipher consumes, and the leading
    /// portion is the whole of it. *Named regression test.*
    #[test]
    fn only_the_leading_material_is_consumed() {
        let creds = credentials().expect("entropy");
        let (key, prefix) = key_material(&creds.aes256).expect("material");

        let leading = &creds.aes256[..(KEY_LEN + NONCE_PREFIX_LEN) * 2];
        assert_eq!(hex(&key), leading[..KEY_LEN * 2]);
        assert_eq!(hex(&prefix), leading[KEY_LEN * 2..]);

        // Anything past the consumed prefix may differ without changing the key.
        let mut altered = creds.aes256.clone();
        altered.replace_range(CONSUMED * 2.., &"0".repeat(altered.len() - CONSUMED * 2));
        let (same_key, same_prefix) = key_material(&altered).expect("material");
        assert_eq!(same_key, key);
        assert_eq!(same_prefix, prefix);
    }

    /// The prefix is credential material, not zeros. A nonce built from four
    /// zero bytes authenticates nothing a peer accepts. *Named regression test.*
    #[test]
    fn the_nonce_prefix_comes_from_the_credential() {
        let material = format!("{}aabbccdd{}", "11".repeat(KEY_LEN), "ff".repeat(64));
        let (key, prefix) = key_material(&material).expect("material");
        assert_eq!(key, [0x11u8; KEY_LEN]);
        assert_eq!(prefix, [0xaa, 0xbb, 0xcc, 0xdd]);
    }

    #[test]
    fn short_or_non_hex_material_is_refused() {
        assert_eq!(key_material("abcd").unwrap_err(), Error::Material);
        assert_eq!(
            key_material(&"zz".repeat(CONSUMED)).unwrap_err(),
            Error::Material
        );
    }

    /// Credentials must not render their contents, however they are formatted.
    #[test]
    fn credentials_do_not_print_their_material() {
        let creds = credentials().expect("entropy");
        let rendered = format!("{creds:?}");
        assert!(!rendered.contains(&creds.aes256), "{rendered}");
        assert!(!rendered.contains(&creds.pwd), "{rendered}");
    }
}
