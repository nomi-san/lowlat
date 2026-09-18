//! The client: the connecting side of the protocol, below the media.
//!
//! Everything the wire needs is the core's, read from the other end: the
//! session is symmetric and the connectivity engine plays both roles. What is
//! here is what a client does with bytes once they are in order -- takes
//! access units off the video channel for a decoder, reads what the host says
//! on the control channel, and speaks the little a client has to say
//! (docs/10-client.md).
//!
//! The public surface is the C ABI in `lowlat-sdk`; the seam is [`Client`].

#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]
// Tests may panic freely: a failing assertion is the point, and a fixture that
// cannot be built is a broken test rather than hostile input.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )
)]

pub mod config;
pub mod decode;
pub mod driver;
pub mod feed;
pub mod frames;
pub mod input;
pub mod seam;
mod shell;
pub mod sound;

pub use config::Config;
pub use driver::{Driver, Lag, Telemetry};
pub use seam::{Client, Error, Event, Outcome, Peer, Transport};
pub use sound::Sound;

/// Video, stream 0; sound (docs/01-protocol.md section 6).
pub const VIDEO_CHANNEL: u8 = 1;
pub const AUDIO_CHANNEL: u8 = 2;

/// What one fragment carries, and therefore what one ring slot holds. The
/// same floor the host frames to, for the same reason: it is the only size a
/// peer is known to accept before a probe has justified more.
pub const BODY: usize = lowlat_core::DEFAULT_BODY;

/// The video receive ring is the flow-control window, so it must be at least
/// the host's ceiling on outstanding fragments, which tops out at exactly this
/// depth. Smaller drops arriving fragments whose slots still hold undelivered
/// data, which stalls the cumulative count on a healthy link and reads as
/// loss; measured at a threefold throughput cut.
pub const VIDEO_RECV_SLOTS: usize = lowlat_core::channel::RING_SLOTS;
/// Sound is fifty packets a second at four fragments each; this holds five
/// seconds of it, which a loop that drains every pass never approaches.
pub const AUDIO_RECV_SLOTS: usize = 1024;
/// Control we receive holds the host's window plus its longest message.
pub const CONTROL_RECV_SLOTS: usize = 1024;
/// Control we send is a handful of small messages per second.
pub const CONTROL_SEND_SLOTS: usize = 256;

/// The largest access unit that can ever arrive whole.
///
/// **Bounded by the ring, not by the host.** A message has to sit entirely in
/// the receive ring before it can be taken, and the ring refuses a fragment
/// further than its depth past the reader, so no message can exceed the ring's
/// depth times a fragment's body. A buffer larger than this is room nothing
/// can fill; one smaller refuses a message without consuming it, and the
/// stream is over.
pub const UNIT_BYTES: usize = VIDEO_RECV_SLOTS * BODY;
/// Access units waiting for the decoder. Two is the depth every client
/// generation keeps; four leaves a keyframe's announcement and the keyframe
/// itself room behind a picture being decoded.
pub const UNIT_SLOTS: usize = 4;
