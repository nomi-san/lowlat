//! The browser transport: a data channel endpoint on the attempt socket.
//!
//! A browser is a guest over SCTP on DTLS 1.2, on the same per-attempt
//! socket the native transport uses, chosen by the offer and never by bytes
//! on the wire: the native record magic *is* a DTLS application-data header,
//! so nothing in a datagram tells the two apart. Everything above the
//! transport -- the control vocabulary, the media payloads, admission -- is
//! the same; what differs is the pipe, and the pipe is fixed by the browser
//! client that already exists (docs/01-protocol.md 14, docs/00-overview.md
//! D13).
//!
//! Both state machines are sans-IO: fed bytes and told the time by the
//! shell, owning no socket, thread or clock. That is what lets a browser
//! session run under the same fake clock as a native one. **They allocate**,
//! a message and a packet at a time, which is why they live here and not in
//! the core; the cost is measured rather than assumed (docs/02-io-shell.md 7).

mod dtls;
mod sctp;
mod session;

pub use session::{Inbound, Role, WebSession};

/// The association port on both sides. A browser's description names it.
pub const SCTP_PORT: u16 = 5000;

/// The one payload identifier accepted: binary. Everything else -- strings,
/// empty markers, the in-band channel-open protocol -- is dropped, so the
/// streams have to be agreed in advance, and they are.
pub const PPID_BINARY: u32 = 53;

/// The largest datagram the record layer emits: the native default, so the
/// path is asked for nothing the native transport does not already ask.
pub const DTLS_MTU: usize = lowlat_core::DEFAULT_DATAGRAM;

/// What a DTLS 1.2 record adds around an AES-GCM payload: the 13-byte header,
/// the 8-byte explicit nonce and the 16-byte tag.
pub const DTLS12_GCM_OVERHEAD: usize = 37;

/// The largest SCTP packet handed to the record layer.
pub const SCTP_MTU: usize = 1191;

// A packet at the SCTP ceiling, wrapped, fits the datagram ceiling.
const _: () = assert!(SCTP_MTU + DTLS12_GCM_OVERHEAD <= DTLS_MTU);

/// The largest message queued on one stream. A keyframe at a high rate is
/// several hundred kilobytes; this leaves room for one several times that.
pub const MAX_MESSAGE: usize = 4 << 20;

/// Send-queue ceilings, in fragments of the native body size, so a refusal
/// here coincides with what the delivery gate would refuse anyway: the gate's
/// deepest ceiling per stream, and room for every stream at that depth.
pub const STREAM_QUEUE_FRAGMENTS: usize = 4000;

/// Messages held per channel for a reader that has not taken them yet.
/// Beyond this the oldest is dropped and counted, as a full native ring is.
pub const INBOX_DEPTH: usize = 1000;

/// Stream priorities: control ahead of sound ahead of pictures, because a
/// picture that waits is late and an input event that waits is lost time.
pub const PRIORITY_CONTROL: u16 = 512;
pub const PRIORITY_AUDIO: u16 = 384;
pub const PRIORITY_VIDEO: u16 = 256;
