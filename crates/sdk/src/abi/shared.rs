//! The part of the public C ABI both halves share: the signaling seam's
//! types, the event queue's, and the helpers that move text across.
//!
//! Present in a build that carries either half, under the same guard in the
//! header, so an application built for one half sees the same
//! `lowlat_candidate` and `lowlat_event` as one built for the other.

#![allow(non_camel_case_types)]

use core::ffi::c_char;

/// How many reflexive servers a seam may be given, and how long each may be.
///
/// **A fixed array rather than a pointer and a count**, so the structure stays
/// one blittable block with nothing in it to free. Four is already more than
/// any host here has ever been configured with.
pub const LOWLAT_SERVERS_MAX: usize = 4;
/// The longest textual `host:port` for one of them.
pub const LOWLAT_SERVER_MAX: usize = 64;

/// The longest attempt identifier carried across this boundary.
///
/// **A fixed array rather than a pointer**, because nothing crosses here that
/// the application has to free (docs/06-api.md 10). An identifier longer than
/// this is the application's own, so it is truncated on the way out rather
/// than refused: the event still says what happened, and the application
/// already holds the identifier it made up.
pub const LOWLAT_ATTEMPT_MAX: usize = 128;

/// The longest textual address, which is what an address for a peer's
/// signaling to forward has to be anyway.
pub const LOWLAT_ADDRESS_MAX: usize = 46;

/// Which member of an event is the valid one.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_event_type {
    /// A local candidate, to be sent to the peer as it is found.
    LOWLAT_EVENT_CANDIDATE = 1,
    /// Send the peer a candidate marked ready, once.
    LOWLAT_EVENT_READY = 2,
    /// Connectivity completed and media can flow.
    LOWLAT_EVENT_ESTABLISHED = 3,
    /// The attempt is over, with a reason.
    LOWLAT_EVENT_ENDED = 4,
    /// A guest sent its application a message.
    LOWLAT_EVENT_USER_DATA = 5,
    /// What is being captured changed: a different output, or the same one at
    /// a different size.
    LOWLAT_EVENT_CAPTURE_CHANGED = 6,
    /// The guest holding the pointer changed, or nobody holds it now.
    LOWLAT_EVENT_INPUT_OWNER_CHANGED = 7,
    /// The host cannot continue. **Never dropped**, whatever the queue is
    /// doing, because it is the only explanation for everything that stopped.
    LOWLAT_EVENT_FATAL = 8,
    /// The host blocked this client's input, or unblocked it. Client only.
    LOWLAT_EVENT_BLOCKED = 9,
    /// The host ended one stream and not the session. Client only.
    LOWLAT_EVENT_STREAM_ENDED = 10,
    /// The host said which mode it is in. Client only.
    LOWLAT_EVENT_HOST_MODE = 11,
    /// The host put this client into relative mode, or took it out. Client
    /// only.
    LOWLAT_EVENT_RELATIVE = 12,
    /// The host's pointer changed: its picture, its hotspot or its flags.
    /// Client only, minor 9.
    LOWLAT_EVENT_CURSOR = 13,
    /// The host asked a pad to vibrate. Client only, minor 9.
    LOWLAT_EVENT_RUMBLE = 14,
    /// The room as the host describes it, with this client's own number;
    /// the body through the caller's buffer. Client only, minor 9.
    LOWLAT_EVENT_GUEST_LIST = 15,
}

/// Why an attempt finished.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_outcome {
    /// Negotiated, and no path was found.
    LOWLAT_OUTCOME_CONNECTIVITY_FAILED = 1,
    /// The peer stopped answering.
    LOWLAT_OUTCOME_PEER_GONE = 2,
    /// Nothing sent has been acknowledged for the delivery deadline, while
    /// something was outstanding the whole time.
    LOWLAT_OUTCOME_UNDELIVERABLE = 3,
    /// The peer said it was leaving.
    LOWLAT_OUTCOME_PEER_LEFT = 4,
    /// Connected, then never said what it could decode.
    LOWLAT_OUTCOME_NEVER_DECLARED = 5,
    /// The socket could not be driven any further.
    LOWLAT_OUTCOME_TRANSPORT_FAILED = 6,
    /// The control stream could not be read any further.
    LOWLAT_OUTCOME_CONTROL_STALLED = 7,
    /// The host ended it, and `reason` carries what the peer was told.
    LOWLAT_OUTCOME_KICKED = 8,
    /// The browser pipe's security handshake did not complete, the peer was
    /// not the one the offer named, or its association ended with an error.
    LOWLAT_OUTCOME_HANDSHAKE_FAILED = 9,
    /// The host ended the session, and `reason` carries the status it gave.
    /// Client only.
    LOWLAT_OUTCOME_DISCONNECTED = 10,
    /// No decoder can serve the stream: the device is gone, was never
    /// usable, or the stream is one it cannot decode. Client only.
    LOWLAT_OUTCOME_DECODER_FAILED = 11,
}

/// A local candidate for the application to forward.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_candidate_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
    pub address: [c_char; LOWLAT_ADDRESS_MAX],
    pub port: u16,
    /// Whether a reflexive server reported this one.
    pub from_stun: bool,
    /// Whether the exchange should mark it lan: host candidates, and every
    /// IPv6 address -- there is no translation to negotiate on that family
    /// however the address was found. Copy both flags into the signaling
    /// verbatim; the marking is decided here so no application re-derives it.
    pub lan: bool,
}

/// Tell the peer this host is ready to be checked.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_ready_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
}

/// A path was found and media is flowing.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_established_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
    pub address: [c_char; LOWLAT_ADDRESS_MAX],
    pub port: u16,
    pub reserved: [u8; 2],
}

/// The attempt is over.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_ended_event {
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
    pub outcome: lowlat_outcome,
    /// The status on the ending, from whichever side sent it: what the peer
    /// was told when this host ended it, and what the peer said when it left.
    ///
    /// **A negative value from a peer is the far end reporting its own
    /// fault**, and it is the only account of one there is -- a host cannot
    /// see that a guest failed to decode. Zero otherwise, and zero is not a
    /// status anything stops on.
    pub reason: i32,
}

/// What the loop is capturing now.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_capture_changed_event {
    pub width: u32,
    pub height: u32,
    /// The identity of the output being captured, which is what a chooser
    /// marks and what absolute input is expressed against.
    pub output: [c_char; LOWLAT_OUTPUT_MAX],
}

/// Who holds the pointer now.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_input_owner_event {
    /// [`LOWLAT_GUEST_ALL`] -- zero -- when nobody holds it.
    pub guest: u32,
}

/// The host cannot continue.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_fatal_event {
    /// What every guest was told on the way out, in the protocol's own
    /// numbering rather than this API's.
    pub reason: i32,
}

/// The host blocked this client's input, or unblocked it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_blocked_event {
    pub blocked: bool,
}

/// The host ended one stream and left the session up.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_stream_ended_event {
    pub stream: u32,
    /// The status the host gave, in the protocol's own numbering.
    pub status: i32,
}

/// Which mode the host is in.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_host_mode_event {
    pub mode: u32,
}

/// Relative mode entered or left.
///
/// On the way out, where the pointer reappears, in the window's units through
/// the viewport the application set; the application warps its pointer there
/// once, on this transition, and not on every update.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_relative_event {
    pub relative: bool,
    pub x: i32,
    pub y: i32,
}

/// The host's pointer as it last described it.
///
/// **The one event that carries a pointer.** `image` points into a buffer the
/// handle owns and is valid until the next `lowlat_client_poll_events` on
/// that handle; the picture is RGBA, eight bits a channel, `width * height *
/// 4` bytes, rows top to bottom, at its native size with the hotspot in its
/// own pixels. Scaling it to the drawn picture is the application's, by the
/// ratio of its viewport to the picture. `image` is null and `image_update`
/// false when this update carries no picture: a mode or position change, the
/// picture already delivered named again (its `checksum` says so; the
/// application keeps what it was given), or a picture the host named that
/// this client no longer holds (`checksum` zero).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_cursor_event {
    /// Where the pointer reappears on the way out of relative mode, in the
    /// window's units through the viewport the application set.
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// The checksum the host names the pointer's picture by, delivered
    /// with this update or before it; zero when the update names none.
    pub checksum: u32,
    pub image: *const u8,
    pub image_len: u32,
    /// The host's pointer is hidden by an application there.
    pub hidden: bool,
    /// The host wants motion as deltas.
    pub relative: bool,
    /// The host's pointer is withheld because it is being driven by touch;
    /// not relative mode.
    pub suppressed: bool,
    /// A picture is in `image`.
    pub image_update: bool,
}

/// The host asked a pad to vibrate.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_rumble_event {
    /// The pad as this client named it in its own reports.
    pub pad: u32,
    /// The two motors, as the wire carries them: eight bits each.
    pub large: u8,
    pub small: u8,
    pub reserved: [u8; 2],
}

/// The room as the host describes it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_guest_list_event {
    /// This client's own number in the list, which is how it finds itself.
    pub number: u32,
    /// How long the body is, as for an application message.
    pub body_len: u32,
}

/// An application message from a guest, or from the host on the client's side.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_user_data_event {
    /// The guest that sent it. **Zero on a client**, whose messages all come
    /// from the host.
    pub guest: u32,
    /// The sub-identifier, which means whatever the application and its
    /// clients agreed it means. Nothing here reads it.
    pub id: u32,
    /// How long the body is. **Not how much was written**: a caller that
    /// offered no buffer is still told what it chose not to receive.
    pub body_len: u32,
}

/// Whichever event this is.
///
/// A union cannot describe itself, and the tag beside it is what says which
/// member to read.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(missing_debug_implementations)]
pub union lowlat_event_body {
    pub candidate: lowlat_candidate_event,
    pub ready: lowlat_ready_event,
    pub established: lowlat_established_event,
    pub ended: lowlat_ended_event,
    pub user_data: lowlat_user_data_event,
    pub capture_changed: lowlat_capture_changed_event,
    pub input_owner: lowlat_input_owner_event,
    pub fatal: lowlat_fatal_event,
    pub blocked: lowlat_blocked_event,
    pub stream_ended: lowlat_stream_ended_event,
    pub host_mode: lowlat_host_mode_event,
    pub relative: lowlat_relative_event,
    pub cursor: lowlat_cursor_event,
    pub rumble: lowlat_rumble_event,
    pub guest_list: lowlat_guest_list_event,
}

/// One event.
///
/// **The tag is first** so an application that does not recognise a type can
/// skip it without knowing anything about the rest, which is what makes adding
/// a type additive.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(missing_debug_implementations)]
pub struct lowlat_event {
    pub kind: lowlat_event_type,
    /// How many events were dropped since the previous delivery.
    ///
    /// **Carried on the next event rather than reported at the time**, which
    /// is the only place it can be: the drop happened because nobody was
    /// polling.
    pub dropped: u32,
    pub body: lowlat_event_body,
}

/// The longest output identity carried across this boundary.
///
/// Which codec a stream is coded with: what a host is asked to encode, and
/// what a client reports its decoder was built for.
///
/// **Named by an enumeration and carried as an integer**, for the reason
/// [`lowlat_status`] is: the application writes this field, so the value
/// arriving is whatever it wrote.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_codec {
    LOWLAT_CODEC_H264 = 1,
    LOWLAT_CODEC_HEVC = 2,
}

/// How a picture is oriented.
///
/// **The coded picture never rotates.** A host sends the display's
/// orientation with its stream, and the peer presents the picture turned and
/// maps pointer coordinates against it; a client hands the same word out with
/// every picture.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_rotation {
    LOWLAT_ROTATION_NONE = 1,
    LOWLAT_ROTATION_90 = 2,
    LOWLAT_ROTATION_180 = 3,
    LOWLAT_ROTATION_270 = 4,
}

/// **Sized for the longest kind of identity, which is a device path.** These
/// are not display connector names, which are short: the same bound carries
/// the sound server's own name for a device, where a USB output's serial and
/// profile land it past a hundred characters, and a display identity on
/// Windows is an operating-system device path, which is bounded at 260. A name
/// that does not fit is truncated silently and then resolves to nothing, so
/// the bound is set by the worst case rather than by the observed one.
pub const LOWLAT_OUTPUT_MAX: usize = 260;

/// The longest credential this boundary carries.
///
/// **Sized by the largest of them, which is the media key.** It travels as
/// text and measures 254 characters, so anything shorter than this truncates a
/// key into something that decrypts nothing and reports no reason.
pub const LOWLAT_ICE_MAX: usize = 256;

/// The longest fingerprint.
pub const LOWLAT_FINGERPRINT_MAX: usize = 112;

/// Which pipe an attempt speaks.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_transport {
    /// Authenticated records on the attempt socket. The default.
    LOWLAT_TRANSPORT_BUD = 0,
    /// A browser's data channel on the same socket.
    LOWLAT_TRANSPORT_WEB = 1,
}

/// One address a peer might be reachable at.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_candidate {
    /// Set by the caller to `sizeof(lowlat_candidate)`.
    pub size: u32,
    pub port: u16,
    /// **A readiness marker rather than an address**, and whatever address
    /// rides along with it is ignored. A peer may withhold every real
    /// candidate until it has seen one, so an application that never forwards
    /// one negotiates against a peer that never offers anything to check.
    pub sync: bool,
    /// Whether a reflexive server reported this address to the peer. The
    /// path-opening probe goes only toward such a candidate; an address the
    /// peer knows directly needs no path opened ahead of its first check.
    /// Zero is safe when the application cannot say -- the punch still runs,
    /// without the early probe.
    pub reflexive: bool,
    /// The exchange's lan marking, copied verbatim from the peer's
    /// signaling: directly routable, checked without ceremony. When both
    /// this and `reflexive` are set, lan wins. Neither set is a real class
    /// too -- a translated-path guess no server verified -- so zero for both
    /// is safe and means exactly that.
    pub lan: bool,
    pub address: [c_char; LOWLAT_ADDRESS_MAX],
}

/// One side's credentials for an attempt: what a host answers an offer with,
/// and what a client puts in its offer.
///
/// **A host's are generated at approval, not at registration.** They are
/// bound to the socket that was just opened for this attempt, so producing
/// them earlier binds them to nothing. A client's are minted with the attempt,
/// before it has a socket, so its `port` is zero.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_credentials {
    /// Set by the caller to `sizeof(lowlat_credentials)`.
    pub size: u32,
    /// **The port this guest was actually bound to**, which is not necessarily
    /// the one that was asked for: the bind walks when a port is taken, and
    /// takes any port once the walk is exhausted.
    ///
    /// **This is the answer to the port that went in, and it arrives
    /// synchronously.** Candidates carry it too, in the addresses they name,
    /// so nothing has to compose an address from it; what it is for is the
    /// caller that needs the number *now* -- to map it on the gateway, open it
    /// on the firewall, or return it to a pool -- rather than when the first
    /// candidate event arrives.
    pub port: u16,
    pub reserved: u16,
    pub ufrag: [c_char; LOWLAT_ICE_MAX],
    pub pwd: [c_char; LOWLAT_ICE_MAX],
    pub fingerprint: [c_char; LOWLAT_FINGERPRINT_MAX],
    pub aes256: [c_char; LOWLAT_ICE_MAX],
}

/// Read a fixed array back as a string, stopping at the terminator.
///
/// An array with no terminator in it is refused rather than read to its end:
/// the application overran a field, and guessing which half it meant is worse
/// than saying so.
pub(super) fn taken(from: &[c_char]) -> Option<&str> {
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(from.as_ptr().cast(), from.len()) };
    let end = bytes.iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(bytes.get(..end)?).ok()
}

/// Read a caller's NUL-terminated string.
///
/// **Bounded rather than trusted.** A pointer with no terminator inside a
/// sane length is a caller that handed over something that is not a string,
/// and walking it to find out is how a library reads somebody else's memory.
pub(super) unsafe fn read_c_str<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(text.cast(), LOWLAT_ATTEMPT_MAX) };
    let end = bytes.iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(bytes.get(..end)?).ok()
}

/// Copy text into a fixed array, terminated, truncating if it must.
pub(super) fn put(into: &mut [c_char], text: &str) {
    into.fill(0);
    let room = into.len().saturating_sub(1);
    for (slot, byte) in into.iter_mut().zip(text.as_bytes().iter().take(room)) {
        *slot = *byte as c_char;
    }
}

/// Say the address and port of a socket into an event's fields.
pub(super) fn put_address(into: &mut [c_char], port: &mut u16, addr: &std::net::SocketAddr) {
    put(into, &addr.ip().to_string());
    *port = addr.port();
}
