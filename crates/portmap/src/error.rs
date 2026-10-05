//! Error type for the gateway's messages.
//!
//! Codes and small `Copy` payloads, never formatted strings: a malformed answer
//! from the local network is routine, and refusing one allocates nothing.

/// Every way a gateway's message can be refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// Shorter than the fixed part of its kind.
    Short,
    /// Longer than any message of its kind may be.
    Long,
    /// A protocol version this side does not speak, as the answer carried it.
    Version(u8),
    /// A field, or a combination of fields, the protocol does not define.
    Malformed,
    /// Not an HTTP response: no status line, or a header block that does not
    /// parse.
    Http,
    /// A redirect, with its status. Refused rather than followed.
    Redirect(u16),
    /// A header block or a body past its cap.
    TooLarge,
    /// The connection closed before the end its framing promised.
    Truncated,
    /// A body framing or coding this side does not take.
    Framing,
    /// A URL this side does not follow: not plain HTTP, not an IPv4 address,
    /// or a byte that has no place in a request line.
    Url,
    /// XML that does not parse, or lacks the element an answer needs.
    Xml,
    /// A status that is neither a success nor carries a fault.
    Status(u16),
}

impl Error {
    /// Stable short name, for logs. Never allocates.
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::Short => "short",
            Error::Long => "long",
            Error::Version(_) => "version",
            Error::Malformed => "malformed",
            Error::Http => "http",
            Error::Redirect(_) => "redirect",
            Error::TooLarge => "too-large",
            Error::Truncated => "truncated",
            Error::Framing => "framing",
            Error::Url => "url",
            Error::Xml => "xml",
            Error::Status(_) => "status",
        }
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub type Result<T> = core::result::Result<T, Error>;
