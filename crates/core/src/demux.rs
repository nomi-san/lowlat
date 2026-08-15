//! Which of the two protocols a datagram belongs to.
//!
//! Connectivity checks and encrypted media share one socket for the life of a
//! session, so every datagram is classified on its first two bytes before
//! anything else touches it. See docs/01-protocol.md 2.
//!
//! The rule is the peer's, and it is asymmetric on purpose: anything that does
//! not look like a check is treated as a record, so a malformed check is
//! rejected by the record layer's authentication rather than by a parser that
//! has already been handed attacker-shaped input.

/// What a datagram was classified as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Datagram {
    /// A connectivity check. Hand it to the connectivity engine.
    Check,
    /// An encrypted record. Hand it to the session.
    Record,
}

/// Classify a received datagram.
///
/// A check has a message type whose first byte is 0 or 1 and whose second byte
/// is 1, which covers a binding request and both binding responses. A record
/// begins `0x17`, well outside that range, so the two cannot collide and no
/// record type may ever be introduced whose first byte is 0 or 1.
pub fn classify(datagram: &[u8]) -> Datagram {
    match (datagram.first(), datagram.get(1)) {
        (Some(&high), Some(&low)) if high <= 1 && low == 1 && datagram.len() > 2 => Datagram::Check,
        _ => Datagram::Record,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::ENVELOPE_LEN;
    use crate::stun::{self, TransactionId};

    #[test]
    fn a_binding_request_classifies_as_a_check() {
        let mut buf = [0u8; 64];
        let len = stun::encode_reflexive_request(&mut buf, TransactionId([1u8; 12])).unwrap();
        assert_eq!(classify(&buf[..len]), Datagram::Check);
    }

    #[test]
    fn a_binding_response_classifies_as_a_check() {
        assert_eq!(classify(&[0x01, 0x01, 0x00, 0x00]), Datagram::Check);
    }

    /// The record magic is outside the range a check's message type can
    /// occupy, which is the whole reason one socket can carry both.
    #[test]
    fn a_record_classifies_as_a_record() {
        let mut record = [0u8; ENVELOPE_LEN + 8];
        record[0] = 0x17;
        record[1] = 0xFE;
        record[2] = 0xFD;
        assert_eq!(classify(&record), Datagram::Record);
    }

    #[test]
    fn a_runt_is_never_a_check() {
        for len in 0..=2usize {
            let bytes = [0x00, 0x01, 0x00];
            assert_eq!(
                classify(&bytes[..len]),
                Datagram::Record,
                "a datagram too short to classify must not reach the check parser"
            );
        }
    }

    /// Anything unrecognised goes to the record layer, where authentication
    /// rejects it. The check parser never sees input that was not shaped like a
    /// check.
    #[test]
    fn an_unknown_first_byte_goes_to_the_record_layer() {
        assert_eq!(classify(&[0x02, 0x01, 0x00, 0x00]), Datagram::Record);
        assert_eq!(classify(&[0x00, 0x02, 0x00, 0x00]), Datagram::Record);
        assert_eq!(classify(&[0xFF, 0xFF, 0xFF, 0xFF]), Datagram::Record);
    }
}
