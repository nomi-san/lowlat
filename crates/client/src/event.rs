//! What the client tells the application, and what it refuses: the events a
//! session raises, why an attempt ended, what a call can refuse, and what
//! reaches the session loop from outside it.
//!
//! Shared by the seam, the session loop, the decode thread and the driver.
//! Nothing here opens a socket or a device, so it builds wherever the driver
//! does.

use std::net::SocketAddr;

use lowlat_common::events::Queued;
use lowlat_core::conn::Kind;

/// Which pipe an attempt asks for. A client of this library uses the native
/// one; the browser's exists so a page can be a guest, and a native client
/// has no reason to be one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Bud,
    Web,
}

/// What the answer told us about the host.
#[derive(Debug, Clone)]
pub struct Peer {
    pub ufrag: String,
    pub pwd: String,
    /// The host's certificate digest: the legacy cipher's key material.
    pub fingerprint: String,
    /// The host's media key, absent from a legacy generation's answer.
    pub aes256: Option<String>,
}

/// What the application must forward to the host, or act on.
///
/// Exhaustive on purpose: the C boundary translates every variant, and one
/// added here must fail to compile there rather than fall through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A local candidate, to be sent to the host as it is found.
    Candidate {
        addr: SocketAddr,
        from_stun: bool,
        lan: bool,
    },
    /// Send the host a candidate marked `sync`, once, after every real one.
    Ready,
    /// Connectivity completed; the initialization has gone out.
    Established { addr: SocketAddr },
    /// The attempt is over, with the reason typed.
    Ended { outcome: Outcome },
    /// The host blocked this client's input, or unblocked it.
    Blocked { blocked: bool },
    /// The host ended one stream and not the session.
    StreamEnded { stream: u32, status: i32 },
    /// The host said which mode it is in.
    HostMode { mode: u32 },
    /// The host's application sent a message. Opaque here.
    UserData { id: u32, text: Vec<u8> },
    /// The host put this client into relative mode, or took it out; on the
    /// way out, where the pointer reappears, in the window's units.
    Relative { relative: bool, x: i32, y: i32 },
    /// The host's pointer changed: its picture, its hotspot or its flags.
    /// The picture, when one came or was named from the cache, travels as
    /// it did on the wire and is decoded at the boundary, on the poller's
    /// thread, into the buffer the application is lent.
    Cursor {
        /// Where the pointer reappears on the way out of relative mode, in
        /// the window's units.
        x: i32,
        y: i32,
        width: u16,
        height: u16,
        /// In the picture's own pixels.
        hot_x: u16,
        hot_y: u16,
        hidden: bool,
        relative: bool,
        suppressed: bool,
        /// The checksum the picture is named by; zero when none is delivered.
        checksum: u32,
        /// The picture as it travelled; empty when none is delivered.
        png: Vec<u8>,
    },
    /// The host asked a pad to vibrate: the pad this client named, and the
    /// two motors as the wire carries them.
    Rumble { pad: u32, large: u8, small: u8 },
    /// What the host's device was written, for a pad this client sends as
    /// its own reports: an output report in the pad's own framing, or a
    /// feature write as the host sent it; `len` bytes of `report`.
    PadReport {
        pad: u32,
        kind: lowlat_core::pad::OutputKind,
        len: u8,
        report: [u8; lowlat_core::pad::REPORT_MAX],
    },
    /// The room as the host describes it, with this client's own number.
    /// The body is the host's application's and is not read here.
    GuestList { number: u32, body: Vec<u8> },
}

impl Queued for Event {
    fn body(&self) -> &[u8] {
        match self {
            Event::UserData { text, .. } => text,
            Event::GuestList { body, .. } => body,
            _ => &[],
        }
    }

    /// The picture is not a body: it is decoded at the boundary into the
    /// buffer the application is lent, never copied into the caller's.
    fn held(&self) -> usize {
        match self {
            Event::Cursor { png, .. } => png.len(),
            _ => 0,
        }
    }
}

/// Why an attempt finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Negotiated, and no path was found.
    ConnectivityFailed,
    /// The host stopped answering.
    PeerGone,
    /// Nothing sent has been acknowledged for the delivery deadline.
    Undeliverable,
    /// The socket or the loop failed.
    TransportFailed,
    /// The host ended the session and said why, with its own status.
    Disconnected(i32),
    /// A message arrived that this client cannot take: larger than any
    /// buffer, or unreadable. The channel cannot advance past it.
    Unreadable,
    /// No decoder can serve the stream: the device is gone, was never
    /// usable, or the stream is one it cannot decode.
    DecoderFailed,
    /// The relay did not answer in time to allocate and permit.
    RelayUnreachable,
    /// The relay refused the credentials or the allocation, or is full. The
    /// same relay refuses again.
    RelayRefused,
    /// A renewal was refused, or went unanswered until what it renewed
    /// lapsed; the relay has let the allocation go.
    RelayLost,
}

/// What a seam call can refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// There is already an attempt; a client makes one at a time.
    Busy,
    /// No attempt by that name.
    UnknownAttempt,
    /// The attempt has already begun.
    AlreadyBegun,
    /// The browser pipe was asked for; a native client does not use it.
    Transport,
    /// The answer's credentials cannot key a session: no media key on an
    /// offer that carried one, or material that does not decode.
    Credentials,
    /// The platform would not supply entropy.
    Crypto,
    /// A socket or thread could not be had.
    Io,
    /// No decoder, with the stage that refused.
    Decoder(DecoderStage),
    /// The application holds as many pictures as it may.
    TooManyHeld,
    /// No session to take pictures from.
    NoSession,
    /// The buffer given holds fewer frames than the sound packet; carries
    /// how many it needs. The packet waits for the next call.
    TooSmall(usize),
    /// A pad's report is not one this path carries: the wrong length or
    /// identifier for its product, a feature report other than calibration
    /// or firmware, or a wireless checksum that does not verify.
    Report,
    /// A pad already sent as the other family: as its own reports, or as
    /// states and buttons. One family until it is unplugged.
    PadFamily,
}

/// Where building a decoder stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderStage {
    /// The runtime library is not on the machine.
    Runtime,
    /// No render node opened.
    Device,
    /// The device decodes none of the profiles a stream could use.
    Profile,
    /// A backend or a frame kind that is not built.
    Unsupported,
    /// A codec library was found and is not one this library may load.
    Licence,
}

/// What reaches the loop from outside it.
// The seam and the loop that pass these are built on Linux so far.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum Arrival {
    Candidate(SocketAddr, Kind),
    PeerReady,
}

/// What the application asks the loop to say.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) enum Ask {
    UserData(u32, Vec<u8>),
    /// Say goodbye and stop.
    Leave,
}

/// How long a departure is given to reach the host before the loop stops.
/// The message is on a reliable channel and needs time to get there.
pub(crate) const LEAVE_GRACE_MS: f64 = 250.0;
