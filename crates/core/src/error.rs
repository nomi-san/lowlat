//! Error type for the protocol core.
//!
//! Errors carry codes and small `Copy` payloads, never formatted strings, so
//! nothing on a failure path allocates. Rejecting a packet is routine, not
//! exceptional: hostile and corrupt input is the normal case on a network.

/// Every way the core can refuse an input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Datagram shorter than the record envelope.
    ShortDatagram,
    /// Authentication failed. The packet is forged, corrupt, or keyed wrong.
    Decrypt,
    /// Cleartext shorter than a packet header.
    ShortPacket,
    /// Marker byte, channel index, sequence, or flag combination is invalid.
    Malformed,
    /// A group acknowledgement arrived shorter than its fixed size.
    ShortAck,
    /// The caller's output buffer cannot hold the result.
    BufferTooSmall,
    /// A message length prefix disagrees with the fragments carrying it.
    BadLength,
    /// A datagram at or beyond the absolute ceiling. Emitting one is a bug on
    /// our side; the peer would discard it whole.
    Oversized,
    /// Key material was not a supported length.
    BadKeyLength,
}

impl Error {
    /// Stable short name, for logs. Never allocates.
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::ShortDatagram => "short-datagram",
            Error::Decrypt => "decrypt",
            Error::ShortPacket => "short-packet",
            Error::Malformed => "malformed",
            Error::ShortAck => "short-ack",
            Error::BufferTooSmall => "buffer-too-small",
            Error::BadLength => "bad-length",
            Error::Oversized => "oversized",
            Error::BadKeyLength => "bad-key-length",
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub type Result<T> = core::result::Result<T, Error>;
