//! The certificate a process presents on the browser transport.
//!
//! One identity for the life of the process, minted on first use. A browser
//! trusts it by fingerprint alone: the credential exchange carries the digest
//! of this certificate, and the peer compares what the handshake shows it
//! against what signaling told it. Chain, name and expiry are never checked
//! by the peer, so the certificate is a container for a key pair and nothing
//! more, which is why it carries no extensions.
//!
//! The key scalar is drawn from this crate's own entropy, so the identity is
//! made where every other secret is made.

use core::fmt;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use p256::ecdsa::{DerSignature, SigningKey};
use p256::pkcs8::EncodePrivateKey;
use sha2::{Digest, Sha256};
use x509_cert::builder::{Builder, CertificateBuilder, Profile};
use x509_cert::name::Name;
use x509_cert::serial_number::SerialNumber;
use x509_cert::spki::SubjectPublicKeyInfoOwned;
use x509_cert::time::{Time, Validity};
use zeroize::Zeroizing;

use crate::{Error, fill};

/// Bytes of a SHA-256 digest.
pub const FINGERPRINT_LEN: usize = 32;

/// The hash name a fingerprint travels with in the credential exchange.
const HASH_NAME: &str = "sha-256";

/// How long the certificate is good for. The peer never checks it, and the
/// process that made it does not live that long.
const VALID_FOR: Duration = Duration::from_secs(30 * 24 * 3600);

/// How far into the past the validity starts, so a peer whose clock trails
/// ours still sees a certificate already in force.
const VALID_FROM_BEFORE: Duration = Duration::from_secs(24 * 3600);

/// A self-signed certificate and the key it certifies.
pub struct Certificate {
    der: Vec<u8>,
    key_pkcs8: Zeroizing<Vec<u8>>,
    fingerprint: [u8; FINGERPRINT_LEN],
}

impl fmt::Debug for Certificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificate")
            .field("der", &self.der.len())
            .field("key", &"<redacted>")
            .field("fingerprint", &format_fingerprint(&self.fingerprint))
            .finish()
    }
}

impl Certificate {
    /// The certificate, DER encoded.
    pub fn der(&self) -> &[u8] {
        &self.der
    }

    /// The private key, PKCS#8 DER encoded.
    pub fn key_pkcs8(&self) -> &[u8] {
        &self.key_pkcs8
    }

    /// SHA-256 of the DER certificate.
    pub fn fingerprint(&self) -> &[u8; FINGERPRINT_LEN] {
        &self.fingerprint
    }

    /// The fingerprint as the credential exchange carries it:
    /// `sha-256 AB:CD:...`, uppercase, colon separated, with the hash name.
    pub fn fingerprint_sdp(&self) -> String {
        format_fingerprint(&self.fingerprint)
    }
}

/// The process certificate, made on the first call and kept.
pub fn certificate() -> Result<&'static Certificate, Error> {
    static CERTIFICATE: OnceLock<Result<Certificate, Error>> = OnceLock::new();
    match CERTIFICATE.get_or_init(mint) {
        Ok(certificate) => Ok(certificate),
        Err(error) => Err(*error),
    }
}

fn mint() -> Result<Certificate, Error> {
    // A random scalar is a valid key with overwhelming probability; the loop
    // covers the values the curve order excludes.
    let key = loop {
        let mut scalar = Zeroizing::new([0u8; 32]);
        fill(scalar.as_mut())?;
        if let Ok(key) = SigningKey::from_slice(scalar.as_ref()) {
            break key;
        }
    };

    let mut serial = [0u8; 16];
    fill(&mut serial)?;
    // A serial is a positive integer; clearing the top bit keeps it one.
    serial[0] &= 0x7F;
    let serial = SerialNumber::new(&serial).map_err(|_| Error::Certificate)?;

    // The one wall-clock read in the workspace, for a validity window that
    // nothing on our side ever compares against.
    let now = SystemTime::now();
    let not_before = now
        .checked_sub(VALID_FROM_BEFORE)
        .and_then(|t| Time::try_from(t).ok())
        .ok_or(Error::Certificate)?;
    let not_after = now
        .checked_add(VALID_FOR)
        .and_then(|t| Time::try_from(t).ok())
        .ok_or(Error::Certificate)?;
    let validity = Validity {
        not_before,
        not_after,
    };

    let subject: Name = "CN=lowlat".parse().map_err(|_| Error::Certificate)?;
    let spki = SubjectPublicKeyInfoOwned::from_key(*key.verifying_key())
        .map_err(|_| Error::Certificate)?;

    // Self-signed, subject as issuer, no extensions: the peer checks only the
    // digest, and an extension it did not ask for is one more thing to refuse.
    let builder = CertificateBuilder::new(
        Profile::Manual { issuer: None },
        serial,
        validity,
        subject,
        spki,
        &key,
    )
    .map_err(|_| Error::Certificate)?;
    let certificate = builder
        .build::<DerSignature>()
        .map_err(|_| Error::Certificate)?;
    let der = x509_cert::der::Encode::to_der(&certificate).map_err(|_| Error::Certificate)?;

    let key_pkcs8 = key.to_pkcs8_der().map_err(|_| Error::Certificate)?;
    let key_pkcs8 = Zeroizing::new(key_pkcs8.as_bytes().to_vec());

    let fingerprint = fingerprint_of(&der);
    Ok(Certificate {
        der,
        key_pkcs8,
        fingerprint,
    })
}

/// SHA-256 of a DER certificate, which is what a fingerprint is.
pub fn fingerprint_of(der: &[u8]) -> [u8; FINGERPRINT_LEN] {
    Sha256::digest(der).into()
}

/// `sha-256 AB:CD:...`: the hash name, a space, then uppercase pairs joined
/// by colons.
pub fn format_fingerprint(fingerprint: &[u8; FINGERPRINT_LEN]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(HASH_NAME.len() + 1 + FINGERPRINT_LEN * 3);
    out.push_str(HASH_NAME);
    out.push(' ');
    for (index, byte) in fingerprint.iter().enumerate() {
        if index > 0 {
            out.push(':');
        }
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0F)]));
    }
    out
}

/// Read a fingerprint as a peer writes it.
///
/// The hash name is optional and case does not matter, since one peer writes
/// the pairs in upper case and another may not. Anything but SHA-256, or a
/// digest of any other length, is refused: a fingerprint that cannot be
/// compared is one the handshake would accept anything against.
pub fn parse_fingerprint(text: &str) -> Result<[u8; FINGERPRINT_LEN], Error> {
    let text = text.trim();
    let digits = match text.split_once(' ') {
        Some((name, rest)) => {
            if !name.eq_ignore_ascii_case(HASH_NAME) {
                return Err(Error::Fingerprint);
            }
            rest.trim()
        }
        None => text,
    };
    let mut out = [0u8; FINGERPRINT_LEN];
    let mut count = 0;
    for pair in digits.split(':') {
        let pair = pair.as_bytes();
        if pair.len() != 2 || count >= FINGERPRINT_LEN {
            return Err(Error::Fingerprint);
        }
        let high = crate::nibble(pair[0]).map_err(|_| Error::Fingerprint)?;
        let low = crate::nibble(pair[1]).map_err(|_| Error::Fingerprint)?;
        out[count] = (high << 4) | low;
        count += 1;
    }
    if count != FINGERPRINT_LEN {
        return Err(Error::Fingerprint);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::signature::Verifier;
    use p256::pkcs8::DecodePrivateKey;
    use x509_cert::der::Decode;

    #[test]
    fn certificate_is_one_per_process() {
        let first = certificate().expect("certificate");
        let second = certificate().expect("certificate");
        assert!(core::ptr::eq(first, second));
        assert_eq!(first.fingerprint(), second.fingerprint());
    }

    #[test]
    fn fingerprint_round_trips_with_and_without_its_prefix() {
        let digest: [u8; FINGERPRINT_LEN] =
            core::array::from_fn(|i| u8::try_from(i * 9 % 256).unwrap());
        let text = format_fingerprint(&digest);
        assert!(text.starts_with("sha-256 "));
        assert_eq!(text.len(), 8 + FINGERPRINT_LEN * 3 - 1);
        assert_eq!(parse_fingerprint(&text).unwrap(), digest);

        let bare = &text[8..];
        assert_eq!(parse_fingerprint(bare).unwrap(), digest);
        assert_eq!(
            parse_fingerprint(&bare.to_ascii_lowercase()).unwrap(),
            digest
        );
        assert_eq!(
            parse_fingerprint(&text.to_ascii_lowercase()).unwrap(),
            digest
        );

        assert_eq!(parse_fingerprint("").unwrap_err(), Error::Fingerprint);
        assert_eq!(
            parse_fingerprint(&bare[..bare.len() - 3]).unwrap_err(),
            Error::Fingerprint
        );
        assert_eq!(
            parse_fingerprint(&format!("{text}:AA")).unwrap_err(),
            Error::Fingerprint
        );
        assert_eq!(
            parse_fingerprint(&format!("sha-1 {bare}")).unwrap_err(),
            Error::Fingerprint
        );
        assert_eq!(
            parse_fingerprint(&bare.replace("A", "G")).unwrap_err(),
            Error::Fingerprint
        );
    }

    /// The DER must parse as a certificate, carry the key that signed it, and
    /// the key must load back from its PKCS#8 form. A format the handshake
    /// library cannot read shows up here rather than as a panic in a session.
    #[test]
    fn the_certificate_parses_and_its_key_signs() {
        let certificate = certificate().expect("certificate");
        let parsed = x509_cert::Certificate::from_der(certificate.der()).expect("a certificate");
        assert_eq!(
            parsed.tbs_certificate.subject,
            parsed.tbs_certificate.issuer
        );
        assert!(parsed.tbs_certificate.extensions.is_none());

        let key = SigningKey::from_pkcs8_der(certificate.key_pkcs8()).expect("the key loads");
        let spki = &parsed.tbs_certificate.subject_public_key_info;
        let public =
            p256::ecdsa::VerifyingKey::from_sec1_bytes(spki.subject_public_key.raw_bytes())
                .expect("a P-256 public key");
        assert_eq!(&public, key.verifying_key());

        // The self-signature verifies under the certified key.
        let tbs = x509_cert::der::Encode::to_der(&parsed.tbs_certificate).unwrap();
        let signature =
            DerSignature::from_bytes(parsed.signature.raw_bytes()).expect("a DER signature");
        public
            .verify(&tbs, &signature)
            .expect("the self-signature holds");

        assert_eq!(
            certificate.fingerprint_sdp(),
            format_fingerprint(&fingerprint_of(certificate.der()))
        );
    }

    #[test]
    fn the_certificate_does_not_print_its_key() {
        let certificate = certificate().expect("certificate");
        let rendered = format!("{certificate:?}");
        assert!(rendered.contains("redacted"), "{rendered}");
        assert!(
            rendered.contains(&certificate.fingerprint_sdp()),
            "{rendered}"
        );
    }
}
