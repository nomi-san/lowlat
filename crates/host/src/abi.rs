//! The public C ABI.
//!
//! The only public surface there is ([06-api.md](../../../docs/06-api.md)).
//! Naming follows the header rather than Rust convention, which is permitted
//! here and nowhere else.

#![allow(non_camel_case_types)]

use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use crate::admission::{Event, Outcome};
use crate::events::Delivery;
use lowlat_event_type::*;
use lowlat_outcome::*;
use lowlat_status::*;

/// A status code.
///
/// **An enumeration for the names and a plain integer wherever one is
/// accepted.** Grouping the codes under a type is what tells a reader that
/// `LOWLAT_TIMEOUT` is a status and `LOWLAT_ATTEMPT_MAX` is a size; taking one
/// back by value as this type would be something else entirely, because
/// reading a discriminant nothing defined is undefined behaviour and an
/// application is free to hand back any integer it has.
///
/// Zero succeeds, positive is a non-fatal condition, negative is an error, and
/// the error space is partitioned by subsystem so that a number says where it
/// came from without a lookup:
///
/// ```text
///   -1 to -99      the boundary itself: arguments, state, contained faults
///   -100 to -199   signaling and admission
///   -200 to -299   capture
///   -300 to -399   encode
///   -400 to -499   transport
/// ```
///
/// A value is assigned once and never reused, including for a condition that
/// is removed.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_status {
    /// The call succeeded.
    LOWLAT_OK = 0,
    /// No event arrived within the timeout. Not an error.
    LOWLAT_TIMEOUT = 1,
    /// A fault was contained at the boundary. The handle no longer runs.
    LOWLAT_ERR_INTERNAL = -1,
    /// An argument was missing, out of range, or contradicted another.
    LOWLAT_ERR_INVALID_ARGUMENT = -2,
    /// The buffer was too small. What it would have taken has been written
    /// back, and nothing has been consumed.
    LOWLAT_ERR_TOO_SMALL = -3,
    /// A previous call was contained at the boundary, so this handle is no
    /// longer trusted to describe its own state. Only destroying it still
    /// works.
    LOWLAT_ERR_POISONED = -4,
    /// This handle is already hosting. Stopping first is the way to start
    /// again with a different configuration.
    LOWLAT_ERR_ALREADY_STARTED = -5,
    /// This handle is not hosting, so there is nothing for the call to act on.
    LOWLAT_ERR_NOT_STARTED = -6,

    /// Every seat is taken. **The offer should be declined**, not left
    /// unanswered: silence reads to a peer as a host still thinking about it.
    LOWLAT_ERR_AT_CAPACITY = -100,
    /// No attempt with that identifier.
    LOWLAT_ERR_UNKNOWN_ATTEMPT = -101,
    /// The attempt has already been approved.
    LOWLAT_ERR_ALREADY_BEGUN = -102,
    /// Withdrawn before it was registered, so it was over before it began. A
    /// withdrawal can overtake the offer it withdraws.
    LOWLAT_ERR_WITHDRAWN = -103,
    /// A socket could not be opened, or a thread could not be started.
    LOWLAT_ERR_IO = -104,
    /// Credentials could not be produced.
    LOWLAT_ERR_CRYPTO = -105,
    /// No guest with that number is connected.
    LOWLAT_ERR_UNKNOWN_GUEST = -106,

    /// Nothing is lit. There is no display to capture: a headless machine, or
    /// one whose session has not started.
    LOWLAT_ERR_NO_DISPLAY = -200,
    /// A display is lit and its framebuffer cannot be reached, which is what
    /// this process is allowed to do rather than what the machine has.
    LOWLAT_ERR_DISPLAY_UNREACHABLE = -201,
}

/// The major version, raised only when something already published changes.
pub const LOWLAT_ABI_MAJOR: u32 = 0;
/// The minor version, raised when surface is appended.
pub const LOWLAT_ABI_MINOR: u32 = 1;

/// Major and minor, packed.
///
/// **The one function whose signature can never change**, because it is what a
/// loader calls to decide whether it may call anything else.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_abi_version() -> u32 {
    (LOWLAT_ABI_MAJOR << 16) | LOWLAT_ABI_MINOR
}

/// What each status says about itself.
///
/// A table rather than a match, because the value arriving is an integer and
/// not necessarily one of these.
const DESCRIPTIONS: [(lowlat_status, &CStr); 17] = [
    (LOWLAT_OK, c"ok"),
    (LOWLAT_TIMEOUT, c"no event within the timeout"),
    (
        LOWLAT_ERR_INTERNAL,
        c"a fault was contained at the boundary",
    ),
    (LOWLAT_ERR_INVALID_ARGUMENT, c"an argument was not usable"),
    (LOWLAT_ERR_TOO_SMALL, c"the buffer was too small"),
    (LOWLAT_ERR_POISONED, c"the handle is poisoned"),
    (
        LOWLAT_ERR_ALREADY_STARTED,
        c"this handle is already hosting",
    ),
    (LOWLAT_ERR_NOT_STARTED, c"this handle is not hosting"),
    (LOWLAT_ERR_AT_CAPACITY, c"every seat is taken"),
    (
        LOWLAT_ERR_UNKNOWN_ATTEMPT,
        c"no attempt with that identifier",
    ),
    (
        LOWLAT_ERR_ALREADY_BEGUN,
        c"the attempt was already approved",
    ),
    (
        LOWLAT_ERR_WITHDRAWN,
        c"the attempt was withdrawn before it was registered",
    ),
    (LOWLAT_ERR_IO, c"a socket or thread could not be created"),
    (LOWLAT_ERR_CRYPTO, c"credentials could not be produced"),
    (LOWLAT_ERR_UNKNOWN_GUEST, c"no guest with that number"),
    (LOWLAT_ERR_NO_DISPLAY, c"nothing is lit"),
    (
        LOWLAT_ERR_DISPLAY_UNREACHABLE,
        c"a display is lit and its framebuffer cannot be reached",
    ),
];

/// Describe a status.
///
/// **It takes a plain integer rather than the enumeration**, so that a value
/// from anywhere can be described -- including one this version of the library
/// does not define, which is exactly the case an application reaches for this
/// in. Passing a status to it is an ordinary widening conversion.
///
/// The pointer is to storage that outlives the library, so it is never freed
/// and never copied out of.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_status_string(status: i32) -> *const c_char {
    let text: &CStr = DESCRIPTIONS
        .iter()
        .find(|(code, _)| *code as i32 == status)
        .map_or(c"unknown status", |(_, text)| text);
    text.as_ptr()
}

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
#[repr(u32)]
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
}

/// Why an attempt finished.
#[repr(u32)]
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
    pub reserved: u8,
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
    /// What the peer was told, when the outcome is that the host ended it.
    /// Zero otherwise, and zero is not a status a peer stops on.
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

/// An application message from a guest.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_user_data_event {
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
/// **Sized for the longest kind of identity, which is a device path.** These
/// are not display connector names, which are short: the same bound carries
/// the sound server's own name for a device, where a USB output's serial and
/// profile land it past a hundred characters, and a display identity on
/// Windows is an operating-system device path, which is bounded at 260. A name
/// that does not fit is truncated silently and then resolves to nothing, so
/// the bound is set by the worst case rather than by the observed one.
pub const LOWLAT_OUTPUT_MAX: usize = 260;

/// How many reflexive servers a host may be given, and how long each may be.
///
/// **A fixed array rather than a pointer and a count**, so the structure stays
/// one blittable block with nothing in it to free. Four is already more than
/// any host here has ever been configured with.
pub const LOWLAT_SERVERS_MAX: usize = 4;
/// The longest textual `host:port` for one of them.
pub const LOWLAT_SERVER_MAX: usize = 64;

/// Which codec the stream is encoded with.
///
/// **Named by an enumeration and carried as an integer**, for the reason
/// [`lowlat_status`] is: the application writes this field, so the value
/// arriving is whatever it wrote.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_codec {
    LOWLAT_CODEC_H264 = 1,
    LOWLAT_CODEC_HEVC = 2,
}

/// Which encoder to build.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_encoder {
    /// **The default, and the right one.** A conversion target is allocated on
    /// the device the display is on and an encoder belonging to another cannot
    /// take it, so the encoder is a consequence of where the display is rather
    /// than a preference. Choosing one is for forcing a particular encoder on a
    /// machine where either would do.
    LOWLAT_ENCODER_FOLLOW_DISPLAY = 0,
    LOWLAT_ENCODER_OPEN = 1,
    LOWLAT_ENCODER_VENDOR = 2,
}

/// How the display this stream shows is oriented.
///
/// **The coded picture never rotates.** This travels to the peer, which is what
/// presents the picture and what maps pointer coordinates against it.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_rotation {
    LOWLAT_ROTATION_NONE = 1,
    LOWLAT_ROTATION_90 = 2,
    LOWLAT_ROTATION_180 = 3,
    LOWLAT_ROTATION_270 = 4,
}

/// Which congestion control level a session runs at.
///
/// **Zero is the most aggressive, not "off".** Its threshold declares
/// congestion on any stale fragment once the send window passes its floor, and
/// it exists only for compatibility with an older scheme. Sensitive is the
/// default and the one to leave alone.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_cg_level {
    LOWLAT_CG_LEVEL_LEGACY = 0,
    LOWLAT_CG_LEVEL_SENSITIVE = 1,
    LOWLAT_CG_LEVEL_RELAXED = 2,
}

/// The video settings that can change while a host is running.
///
/// **Split out because the split is real.** Everything here is applied without
/// rebuilding anything the session rests on: a bitrate re-bases the budget and
/// reaches the encoder through the reconfigure the rate loop already performs,
/// and a frame rate changes the pacing from the next frame. What is not here --
/// the codec, the encoder, the guest limit, the ports -- is settled when
/// hosting starts, because changing it means building the pipeline again.
///
/// **There is no resolution and no rotation.** The display decides its own size
/// and orientation and this host follows; asking it to be something else is a
/// request to whoever owns the display, which is not this library
/// ([impl-plan](../../../docs/impl-plan.md), *Output selection*).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_host_video_config {
    /// Set by the caller to `sizeof(lowlat_host_video_config)`.
    pub size: u32,
    /// **A ceiling, not a target.** Capture runs at the display's own rate and
    /// this is the most that is encoded from it.
    pub fps: u32,
    /// What the operator asked for, before it is divided among guests.
    pub bitrate_mbps: f64,
    /// The floor congestion control may not descend below. Lowered with the
    /// ceiling when it would otherwise sit above it.
    pub min_bitrate_mbps: f64,
    /// Emit at `fps` even when the picture has not changed.
    ///
    /// **A permission, not an instruction, and off by default.** There is no
    /// damage signal here, so nothing yet skips a repeated picture; a host that
    /// keeps sending costs bitrate rather than being wrong. Setting it promises
    /// to spend that bitrate whatever else becomes possible later.
    pub full_fps: bool,
    pub reserved: [u8; 3],
    /// Which output to capture, by an identity from the enumeration. **Empty
    /// means whichever this host would pick on its own**, which is the output
    /// at the desktop's corner and then whatever is lit.
    pub output: [c_char; LOWLAT_OUTPUT_MAX],
}

/// How sound is configured.
///
/// **Every field here is live.** Sound has no half that must be settled when
/// hosting starts: the device and the mute cost a reconnect the loop performs,
/// and the rest are read on the frame that uses them. So this is both what a
/// host starts with and what [`lowlat_host_set_audio_config`] takes.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct lowlat_host_audio_config {
    /// Set by the caller to `sizeof(lowlat_host_audio_config)`.
    pub size: u32,
    /// What the compressed form is encoded at, in kilobits a second.
    pub bitrate_kbps: u32,
    /// Whether sound is captured at all. Off gives the device back and puts
    /// the speakers at the desk back with it.
    pub enabled: bool,
    /// Whether a guest that asked for the uncompressed form may have it.
    ///
    /// **A permission, not a request**, and off by default: it costs an order
    /// of magnitude more of the uplink than the compressed form, which comes
    /// out of what is left for the picture.
    pub allow_uncompressed: bool,
    /// Silence the speakers at the desk while a guest is connected.
    ///
    /// **On a device that applies its own mute**, the tap is ahead of it: a
    /// guest still hears everything and it is the person at the machine who
    /// stops hearing what they are sending. Restored when the last guest
    /// leaves, and only if this host is what silenced them.
    ///
    /// **On a device whose mute the server applies, this does nothing and says
    /// so.** The mix the mute is applied to is the one the capture reads, so
    /// obeying would silence every guest; asking is not refused, because the
    /// device can change under a running host, but the mute is not performed
    /// while the device is of that kind.
    pub mute_local: bool,
    pub reserved: [u8; 1],
    /// Which device to capture, by an identity from the enumeration. **Empty
    /// means the default output's monitor**, followed as the default changes.
    pub device: [c_char; LOWLAT_OUTPUT_MAX],
}

/// How a host is configured.
///
/// **There is no resolution here.** The display decides the picture's size, the
/// encoder follows it, and the application is told what it got rather than
/// asking for it; `fps` is a cap over whatever the display runs at, not a
/// target. A host that creates its own display chooses that display's size when
/// it creates it, which is a different question and not this field's.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_host_config {
    /// Set by the caller to `sizeof(lowlat_host_config)`.
    pub size: u32,
    /// The base a guest's port bind walks from.
    pub base_port: u16,
    pub reserved: u16,
    /// Advertised capacity. Above [`LOWLAT_GUESTS_MAX`] is refused rather than
    /// quietly reduced.
    pub max_guests: u32,
    /// One of [`lowlat_codec`]. Settled when hosting starts: one encode serves
    /// every seat and a session has one video configuration.
    pub codec: u32,
    /// One of [`lowlat_encoder`].
    pub encoder: u32,
    /// One of [`lowlat_cg_level`].
    pub cg_level: u32,
    /// How long a guest keeps the pointer after its last movement, when
    /// `exclusive_pointer` is set. Clamped rather than refused: this is a
    /// comfort setting and the nearest usable value beats refusing to start.
    pub exclusive_hold_ms: u32,
    /// Whether one guest at a time may drive the pointer. Off means everybody
    /// drives it, which is a configuration rather than a fault.
    pub exclusive_pointer: bool,
    pub reserved2: [u8; 3],
    /// How many of `servers` are set.
    pub server_count: u32,
    /// Reflexive servers, consulted for this host's own mapped address, each
    /// `host:port`.
    pub servers: [[c_char; LOWLAT_SERVER_MAX]; LOWLAT_SERVERS_MAX],
    /// The half of this that can also be set while the host runs.
    pub video: lowlat_host_video_config,
    /// Sound, every field of which can also be set while the host runs.
    pub audio: lowlat_host_audio_config,
}

/// The most guests a host may advertise, which is what the ring memory per
/// guest is sized against.
pub const LOWLAT_GUESTS_MAX: u32 = 16;

/// What a handle is created with.
///
/// **The caller sets `size`.** It is read rather than assumed, so this can grow
/// without breaking an application compiled against an older header
/// (docs/06-api.md 1).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_create_info {
    pub size: u32,
}

/// Everything one handle owns, under one lock.
///
/// **One lock rather than two.** A second would be a lock ordering, and a lock
/// ordering is what produces the first deadlock on the day somebody adds a call
/// that needs both (docs/06-api.md 8).
#[derive(Debug)]
struct Held {
    seam: Option<crate::admission::Admission>,
}

/// The longest credential this boundary carries.
///
/// **Sized by the largest of them, which is the media key.** It travels as
/// text and measures 254 characters, so anything shorter than this truncates a
/// key into something that decrypts nothing and reports no reason.
pub const LOWLAT_ICE_MAX: usize = 256;

/// The longest fingerprint.
pub const LOWLAT_FINGERPRINT_MAX: usize = 112;

/// What a guest may drive.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_permissions {
    pub keyboard: bool,
    pub pointer: bool,
    pub gamepad: bool,
    pub reserved: u8,
}

/// What signaling learned about a peer, handed over to register an attempt.
///
/// **Signaling is the application's**, so everything here arrived over a
/// transport this library does not have and does not want
/// ([04 §1](../../../docs/04-signaling.md)).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_attempt_info {
    /// Set by the caller to `sizeof(lowlat_attempt_info)`.
    pub size: u32,
    pub reserved: u32,
    /// The application's own identifier for this attempt. Everything else in
    /// the seam is addressed by it.
    pub attempt_id: [c_char; LOWLAT_ATTEMPT_MAX],
    pub ufrag: [c_char; LOWLAT_ICE_MAX],
    pub pwd: [c_char; LOWLAT_ICE_MAX],
    /// The peer's media key material, as text.
    ///
    /// **Empty selects the legacy path**, which is a decision rather than a
    /// degradation: the offer either carried one or it did not, and which
    /// crypto a session uses follows from that ([00 §D2](../../../docs/00-overview.md)).
    pub aes256: [c_char; LOWLAT_ICE_MAX],
    /// What signaling says this peer may drive.
    pub permissions: lowlat_permissions,
    /// Whether this peer owns the machine, which decides exactly one thing: it
    /// takes the pointer from another guest rather than waiting for it.
    pub owner: bool,
    pub reserved2: [u8; 3],
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
    pub reserved: u8,
    pub address: [c_char; LOWLAT_ADDRESS_MAX],
}

/// What this host answers an offer with.
///
/// **Generated at approval, not at registration.** They are bound to the
/// socket that was just opened for this attempt, so producing them earlier
/// binds them to nothing.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_credentials {
    /// Set by the caller to `sizeof(lowlat_credentials)`.
    pub size: u32,
    /// **The port this guest was actually bound to**, which is not necessarily
    /// the configured one: the bind walks when a port is taken. Advertising the
    /// configured port instead produces a peer that answers checks and never
    /// establishes.
    pub port: u16,
    pub reserved: u16,
    pub ufrag: [c_char; LOWLAT_ICE_MAX],
    pub pwd: [c_char; LOWLAT_ICE_MAX],
    pub fingerprint: [c_char; LOWLAT_FINGERPRINT_MAX],
    pub aes256: [c_char; LOWLAT_ICE_MAX],
}

/// Every guest at once, where a guest number is taken.
///
/// **Zero, because guest numbers start at one.** A message aimed here reaches
/// everyone seated rather than nobody.
pub const LOWLAT_GUEST_ALL: u32 = 0;

/// One connected guest.
///
/// **No leading `size` field, and it is the one structure that cannot have
/// one.** The caller passes an array of these and walks it by stride, so a
/// size written per element says nothing about how far apart they are; the
/// count is the versioning instead, and this stays fixed for the major
/// version. Anything learned about a guest later arrives through a call of its
/// own rather than by growing this.
///
/// **Every guest here is connected.** One that is still negotiating has no
/// number yet and nothing to address, and the state it passes through is what
/// the guest-state event reports.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_guest {
    /// What this guest is addressed by, and what it finds itself by in a
    /// roster the application sends.
    pub number: u32,
    pub permissions: lowlat_permissions,
    /// Whether this guest owns the machine, which decides exactly one thing:
    /// it takes the pointer from another guest rather than waiting for it.
    pub owner: bool,
    pub reserved: [u8; 3],
    /// The identifier this attempt was registered under.
    ///
    /// **The link between the seam's two halves.** Everything before a guest is
    /// seated is addressed by attempt and everything after is addressed by
    /// number; without this, an application holding one peer per attempt cannot
    /// tell which peer an event about guest three concerns.
    pub attempt: [c_char; LOWLAT_ATTEMPT_MAX],
}

/// How severe a log line is.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_log_level {
    LOWLAT_LOG_ERROR = 0,
    LOWLAT_LOG_WARN = 1,
    LOWLAT_LOG_INFO = 2,
    LOWLAT_LOG_DEBUG = 3,
    LOWLAT_LOG_TRACE = 4,
}

/// Where log lines go.
///
/// **The one place this library calls into an application**, and the single
/// exception to being poll-based. It is cold, it fires on whichever thread
/// logged, and it must not call back in.
pub type lowlat_log_fn =
    Option<unsafe extern "C" fn(level: u32, message: *const c_char, opaque: *mut c_void)>;

/// The callback and whatever the application wanted handed back with it.
///
/// **A lock rather than an atomic pair**, because the two must be read
/// together: a callback taken with the previous registration's opaque pointer
/// would hand an application a pointer belonging to something it has already
/// forgotten. Logging is cold enough to afford it.
static LOGGER: std::sync::Mutex<(lowlat_log_fn, usize)> = std::sync::Mutex::new((None, 0));

/// Hand one already-formatted line to whatever the application registered.
///
/// **Installed once and replaceable behind that**, so an application may
/// change where its logs go without the underlying sink -- which is
/// process-wide and takes one installation -- having to be changed with it.
fn to_application(level: lowlat_common::log::Level, message: &str) {
    let Ok(logger) = LOGGER.lock() else {
        return;
    };
    let (Some(callback), opaque) = *logger else {
        return;
    };
    // **A copy, because a Rust string has no terminator and C reads one.**
    // Logging allocates here and nowhere else on this path; a line with an
    // interior NUL is truncated at it rather than dropped, since a short
    // message beats a lost one.
    let Ok(text) = std::ffi::CString::new(message) else {
        let Ok(truncated) = std::ffi::CString::new(
            message
                .split('\0')
                .next()
                .unwrap_or_default()
                .as_bytes()
                .to_vec(),
        ) else {
            return;
        };
        unsafe { callback(level as u32, truncated.as_ptr(), opaque as *mut c_void) };
        return;
    };
    unsafe { callback(level as u32, text.as_ptr(), opaque as *mut c_void) };
}

/// Receive log messages from every part of this library.
///
/// Passing `NULL` stops delivery and returns the library to writing lines on
/// standard error itself.
///
/// **The callback may be replaced.** The underlying sink is process-wide and
/// installed once; what an application registers here sits behind it, so
/// calling this again changes where lines go rather than being refused.
///
/// # Safety
///
/// `fn_` must remain callable, and `opaque` valid, until this is called again
/// with something else or with `NULL`. It may fire on any thread, and it must
/// not call back into this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_set_log_callback(
    fn_: lowlat_log_fn,
    opaque: *mut c_void,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        {
            let Ok(mut logger) = LOGGER.lock() else {
                return LOWLAT_ERR_INTERNAL;
            };
            *logger = (fn_, opaque as usize);
        }
        // Installed on the first registration and never again; a later one
        // only changes what the shim finds.
        lowlat_common::log::set_sink(to_application);
        LOWLAT_OK
    })
}

/// Set how much is logged. Lines above this level are not formatted at all.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_set_log_level(level: u32) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        let level = match level {
            code if code == lowlat_log_level::LOWLAT_LOG_ERROR as u32 => {
                lowlat_common::log::Level::Error
            }
            code if code == lowlat_log_level::LOWLAT_LOG_WARN as u32 => {
                lowlat_common::log::Level::Warn
            }
            code if code == lowlat_log_level::LOWLAT_LOG_INFO as u32 => {
                lowlat_common::log::Level::Info
            }
            code if code == lowlat_log_level::LOWLAT_LOG_DEBUG as u32 => {
                lowlat_common::log::Level::Debug
            }
            code if code == lowlat_log_level::LOWLAT_LOG_TRACE as u32 => {
                lowlat_common::log::Level::Trace
            }
            _ => return LOWLAT_ERR_INVALID_ARGUMENT,
        };
        lowlat_common::log::set_level(level);
        LOWLAT_OK
    })
}

/// One host session, as the application holds it.
///
/// Opaque: the application holds a pointer it cannot look inside, so what is
/// in here changes freely.
#[derive(Debug)]
pub struct lowlat {
    /// Set when a call was contained, and never cleared.
    poisoned: AtomicBool,
    held: std::sync::Mutex<Held>,
    /// **The queue exists from the moment the handle does**, and outlives any
    /// one host on it. An application starts its polling thread before it
    /// starts hosting, so a poll has something real to wait on from the first
    /// call; and events a host raised on the way down are still there to be
    /// taken after it has stopped.
    ///
    /// Held outside the lock because a poll waits for as long as its caller
    /// asked and every other call must stay answerable while it does.
    events: crate::events::Receiver,
    /// The other end, handed to each host as it starts.
    raise: crate::events::Sender,
}

impl lowlat {
    /// **A poisoned lock is not a second failure to report.** The handle is
    /// already refusing every call once a panic has been contained, and that
    /// is the state this would be describing.
    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|held| held.into_inner())
    }
}

/// Create a handle.
///
/// # Safety
///
/// `out` must point to storage for one pointer. `info` may be null, which
/// takes every default.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_create(
    info: *const lowlat_create_info,
    out: *mut *mut lowlat,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        if out.is_null() {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        // Read only as much as the caller says it wrote. There is nothing
        // beyond the length yet; the check is here because the first version
        // is where the habit is either established or lost.
        if let Some(info) = (unsafe { info.as_ref() })
            && (info.size as usize) < core::mem::size_of::<u32>()
        {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let (raise, events) = crate::events::queue();
        let handle = Box::new(lowlat {
            poisoned: AtomicBool::new(false),
            held: std::sync::Mutex::new(Held { seam: None }),
            events,
            raise,
        });
        unsafe { out.write(Box::into_raw(handle)) };
        LOWLAT_OK
    })
}

/// Destroy a handle.
///
/// **Works on a poisoned handle**, which is the point of poisoning: everything
/// else is refused and this still releases what was taken.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`] and is not used again. A null pointer is
/// accepted and does nothing.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_destroy(ll: *mut lowlat) {
    guard((), || {
        if ll.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(ll) });
    });
}

/// Read a fixed array back as a string, stopping at the terminator.
///
/// An array with no terminator in it is refused rather than read to its end:
/// the application overran a field, and guessing which half it meant is worse
/// than saying so.
fn taken(from: &[c_char]) -> Option<&str> {
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(from.as_ptr().cast(), from.len()) };
    let end = bytes.iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(bytes.get(..end)?).ok()
}

/// Turn what the application wrote into what the seam takes.
///
/// **Every enumerated field is validated rather than transmuted.** The
/// application filled this structure, so each of these is whatever it wrote,
/// and a value nothing defined is refused here instead of becoming a variant
/// that does not exist.
fn configured(cfg: &lowlat_host_config) -> Option<crate::admission::Config> {
    let codec = match cfg.codec {
        code if code == lowlat_codec::LOWLAT_CODEC_H264 as u32 => crate::stream::Codec::H264,
        code if code == lowlat_codec::LOWLAT_CODEC_HEVC as u32 => crate::stream::Codec::H265,
        _ => return None,
    };
    let backend = match cfg.encoder {
        code if code == lowlat_encoder::LOWLAT_ENCODER_FOLLOW_DISPLAY as u32 => None,
        code if code == lowlat_encoder::LOWLAT_ENCODER_OPEN as u32 => {
            Some(crate::stream::Backend::Open)
        }
        code if code == lowlat_encoder::LOWLAT_ENCODER_VENDOR as u32 => {
            Some(crate::stream::Backend::Vendor)
        }
        _ => return None,
    };
    let cg_level = match cfg.cg_level {
        code if code == lowlat_cg_level::LOWLAT_CG_LEVEL_LEGACY as u32 => 0,
        code if code == lowlat_cg_level::LOWLAT_CG_LEVEL_SENSITIVE as u32 => 1,
        code if code == lowlat_cg_level::LOWLAT_CG_LEVEL_RELAXED as u32 => 2,
        _ => return None,
    };
    if cfg.max_guests == 0 || cfg.max_guests > LOWLAT_GUESTS_MAX {
        return None;
    }
    if cfg.server_count as usize > LOWLAT_SERVERS_MAX {
        return None;
    }
    let mut servers = Vec::new();
    let mut dropped = 0usize;
    for server in cfg.servers.iter().take(cfg.server_count as usize) {
        // Resolved here rather than at the first check, so a name that does not
        // resolve is refused while the caller is still holding the call that
        // set it.
        let text = taken(server)?;
        let found = crate::admission::resolve_server(text);
        // A name that resolves to nothing is refused here, while the caller is
        // still holding the call that set it.
        if found.is_empty() {
            return None;
        }
        for addr in found {
            if servers.len() < LOWLAT_SERVERS_MAX {
                servers.push(addr);
            } else {
                dropped += 1;
            }
        }
    }
    // **Said out loud.** Two families per name can exceed what the engine
    // holds, and a cap nobody reports reads as though every server configured
    // is being asked.
    if dropped > 0 {
        lowlat_common::log_warn!(
            "host: reflexive servers capped, kept={} dropped={dropped}",
            servers.len()
        );
    }
    let video = video_configured(&cfg.video)?;
    let output = taken(&cfg.video.output)?;
    let (sound_on, allow_raw, audio_kbps, audio_live) = audio_configured(&cfg.audio)?;

    Some(crate::admission::Config {
        base_port: cfg.base_port,
        // Not on the boundary yet: no application has asked to offer shared
        // address space, and a field nobody sets is a field nobody tests.
        shared_address_space: false,
        max_guests: cfg.max_guests as usize,
        servers,
        exclusive_pointer: cfg.exclusive_pointer,
        exclusive_hold_ms: f64::from(cfg.exclusive_hold_ms),
        cg_level,
        // A live-run aid, and nothing an application should be able to ask for.
        rumble_probe: false,
        stream: Some(crate::stream::Config {
            audio_kbps,
            allow_raw_audio: allow_raw,
            // **The source is always described and the switch is separate.**
            // Sound has no settled half at this boundary, so a host started
            // with it off must be able to turn it on: a source decided once
            // from the starting value would make `enabled` the one field of
            // this structure that is not live.
            audio_on: sound_on,
            audio: Some(lowlat_audio::Config {
                server: None,
                wanted: std::sync::Arc::new(lowlat_audio::Wanted::new(audio_live)),
            }),
            codec,
            backend,
            cg_level,
            // **The generated picture's size, which is not a product
            // feature.** A host started through this boundary captures a
            // display and follows whatever size it is; these exist for running
            // the layers above capture without a screen.
            width: 0,
            height: 0,
            detail_rows: 0,
            display: true,
            // **Followed rather than configured.** The display decides its own
            // orientation and this reports what it found; there is nothing to
            // read it from yet, so it is declared flat until there is.
            rotation: lowlat_core::video::Rotation::None,
            fps: video.fps,
            configured_mbps: video.bitrate_mbps,
            min_mbps: video.min_mbps,
            full_fps: video.full_fps,
            output: (!output.is_empty()).then(|| output.to_string()),
        }),
    })
}

/// Check the half that can also be set while a host runs.
fn video_configured(cfg: &lowlat_host_video_config) -> Option<crate::stream::LiveVideo> {
    if (cfg.size as usize) < core::mem::size_of::<lowlat_host_video_config>() {
        return None;
    }
    if cfg.fps == 0 {
        return None;
    }
    if !(cfg.bitrate_mbps.is_finite() && cfg.bitrate_mbps > 0.0)
        || !(cfg.min_bitrate_mbps.is_finite() && cfg.min_bitrate_mbps > 0.0)
        || cfg.min_bitrate_mbps > cfg.bitrate_mbps
    {
        return None;
    }
    // Read for its terminator even where the value is not wanted here: a field
    // that was overrun is refused rather than half-read.
    taken(&cfg.output)?;
    Some(crate::stream::LiveVideo {
        fps: cfg.fps,
        bitrate_mbps: cfg.bitrate_mbps,
        min_mbps: cfg.min_bitrate_mbps,
        full_fps: cfg.full_fps,
    })
}

/// Turn what an application wrote about sound into what the host takes.
fn audio_configured(
    cfg: &lowlat_host_audio_config,
) -> Option<(bool, bool, u32, lowlat_audio::Live)> {
    if (cfg.size as usize) < core::mem::size_of::<lowlat_host_audio_config>() {
        return None;
    }
    if cfg.bitrate_kbps == 0 || cfg.bitrate_kbps > LOWLAT_AUDIO_KBPS_MAX {
        return None;
    }
    let device = taken(&cfg.device)?;
    Some((
        cfg.enabled,
        cfg.allow_uncompressed,
        cfg.bitrate_kbps,
        lowlat_audio::Live {
            device: (!device.is_empty()).then(|| device.to_owned()),
            mute_local: cfg.mute_local,
        },
    ))
}

/// The most this host will encode sound at.
///
/// **A ceiling rather than a range**, because the codec silently clamps its own
/// and an application that asked for ten megabits would be told yes and given
/// something else. Well above any rate stereo desktop sound is worth.
pub const LOWLAT_AUDIO_KBPS_MAX: u32 = 512;

/// Start hosting.
///
/// Guests are admitted through the signaling seam, which is the application's
/// own; this starts what serves them once they arrive.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `cfg` points to one
/// [`lowlat_host_config`] whose `size` says how much of it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_start(
    ll: *mut lowlat,
    cfg: *const lowlat_host_config,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(cfg) = cfg.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (cfg.size as usize) < core::mem::size_of::<lowlat_host_config>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let mut held = handle.held();
            if held.seam.is_some() {
                return LOWLAT_ERR_ALREADY_STARTED;
            }
            let Some(config) = configured(cfg) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            // **Raising into the handle's queue rather than its own**, so a
            // host starting and stopping does not take the queue with it.
            held.seam = Some(crate::admission::Admission::raising(
                config,
                handle.raise.clone(),
                None,
            ));
            LOWLAT_OK
        })
    }
}

/// Map a seam error onto the status an application reads.
///
/// Exhaustive on purpose: a new way for admission to refuse should break this
/// build rather than reach an application as a generic failure.
fn refused(error: crate::admission::Error) -> lowlat_status {
    use crate::admission::Error;
    match error {
        Error::UnknownAttempt => LOWLAT_ERR_UNKNOWN_ATTEMPT,
        Error::AtCapacity => LOWLAT_ERR_AT_CAPACITY,
        Error::AlreadyBegun => LOWLAT_ERR_ALREADY_BEGUN,
        Error::Withdrawn => LOWLAT_ERR_WITHDRAWN,
        Error::Io => LOWLAT_ERR_IO,
        Error::Crypto => LOWLAT_ERR_CRYPTO,
    }
}

/// Register an attempt from an offer signaling delivered.
///
/// **Registering is not approving.** This takes a seat's worth of bookkeeping
/// and nothing else; no socket is opened and no thread is started until
/// [`lowlat_host_begin_p2p`]. An application that decides to decline simply
/// never calls that, and says so over its own signaling.
///
/// [`LOWLAT_ERR_AT_CAPACITY`] means the offer should be declined rather than
/// left unanswered: nothing in the protocol reports a host that never replied,
/// so a peer given silence sits connecting until its own deadline.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `info` points to one
/// [`lowlat_attempt_info`] whose `size` says how much of it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_new_attempt(
    ll: *mut lowlat,
    info: *const lowlat_attempt_info,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(info) = info.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (info.size as usize) < core::mem::size_of::<lowlat_attempt_info>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let (Some(attempt), Some(ufrag), Some(pwd), Some(aes256)) = (
                taken(&info.attempt_id),
                taken(&info.ufrag),
                taken(&info.pwd),
                taken(&info.aes256),
            ) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            // An attempt with no identifier cannot be addressed again, so every
            // later call about it would find nothing.
            if attempt.is_empty() || ufrag.is_empty() || pwd.is_empty() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let peer = crate::admission::Peer {
                ufrag: ufrag.to_string(),
                pwd: pwd.to_string(),
                aes256: (!aes256.is_empty()).then(|| aes256.to_string()),
                permissions: lowlat_inject::event::Permissions {
                    keyboard: info.permissions.keyboard,
                    pointer: info.permissions.pointer,
                    gamepad: info.permissions.gamepad,
                },
                owner: info.owner,
            };
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            match seam.new_attempt(attempt, peer) {
                Ok(()) => LOWLAT_OK,
                Err(error) => refused(error),
            }
        })
    }
}

/// Offer one address the peer might be reachable at.
///
/// **An unknown attempt is accepted silently.** Candidates trickle and a
/// withdrawal can overtake them, so this is a race with teardown rather than a
/// fault, and a status the caller would have to ignore is worse than no status.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], `attempt_id` is a NUL-terminated string,
/// and `cand` points to one [`lowlat_candidate`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_add_candidate(
    ll: *mut lowlat,
    attempt_id: *const c_char,
    cand: *const lowlat_candidate,
) {
    unsafe {
        entered(ll, |handle| {
            let (Some(attempt), Some(cand)) = (read_c_str(attempt_id), cand.as_ref()) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (cand.size as usize) < core::mem::size_of::<lowlat_candidate>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let Some(address) = taken(&cand.address) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            // **A readiness marker carries no usable address**, so one is not
            // required of it; every other candidate is refused without one
            // rather than checked against nothing.
            let addr = match address.parse::<std::net::IpAddr>() {
                Ok(ip) => std::net::SocketAddr::new(ip, cand.port),
                Err(_) if cand.sync => std::net::SocketAddr::from(([0, 0, 0, 0], cand.port)),
                Err(_) => return LOWLAT_ERR_INVALID_ARGUMENT,
            };
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            seam.add_candidate(attempt, addr, cand.sync);
            LOWLAT_OK
        });
    }
}

/// Approve an attempt and answer it with this host's own credentials.
///
/// This is where a socket is opened and this guest's threads are started, so
/// it is the one call in the seam that costs more than bookkeeping. It sends
/// nothing: the answer travels over the application's signaling, because this
/// library has no transport for it.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], `attempt_id` is a NUL-terminated string,
/// and `out` points to one [`lowlat_credentials`] whose `size` says how much of
/// it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_begin_p2p(
    ll: *mut lowlat,
    attempt_id: *const c_char,
    out: *mut lowlat_credentials,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let (Some(attempt), Some(slot)) = (read_c_str(attempt_id), out.as_mut()) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (slot.size as usize) < core::mem::size_of::<lowlat_credentials>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            let ours = match seam.begin_p2p(attempt) {
                Ok(ours) => ours,
                Err(error) => return refused(error),
            };
            slot.port = ours.port;
            slot.reserved = 0;
            put(&mut slot.ufrag, &ours.ufrag);
            put(&mut slot.pwd, &ours.pwd);
            put(&mut slot.fingerprint, &ours.fingerprint);
            put(&mut slot.aes256, &ours.aes256);
            LOWLAT_OK
        })
    }
}

/// End an attempt, whether or not it was ever approved.
///
/// **An unknown identifier is accepted silently**, and remembered: a
/// withdrawal can arrive before the offer it withdraws, and admitting that
/// offer afterwards spends a socket and a thread on a guest that has already
/// gone.
///
/// **The peer is not told why.** Ending stops this guest's loop; the far side
/// learns from its own liveness deadline rather than from a message, for the
/// same reason [`lowlat_host_stop`] does.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`] and `attempt_id` is a NUL-terminated
/// string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_end_connection(ll: *mut lowlat, attempt_id: *const c_char) {
    unsafe {
        entered(ll, |handle| {
            let Some(attempt) = read_c_str(attempt_id) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            seam.end_connection(attempt);
            LOWLAT_OK
        });
    }
}

/// List the guests that are connected.
///
/// **Two calls, and the caller owns the buffer.** Pass `NULL` for `out` to
/// learn how many there are, then an array of that many. Nothing here is
/// allocated on the application's behalf, so there is nothing to free.
///
/// `count` carries the array's capacity in and the number written out. A
/// buffer smaller than the roster is filled as far as it goes and answered
/// with [`LOWLAT_ERR_TOO_SMALL`], `count` set to what it would have taken --
/// the roster moves, and a caller that sized its array a moment ago must not
/// be made to lose the call.
///
/// # Safety
///
/// `count` must be readable and writable, and `out`, when not null, must point
/// to at least `*count` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_get_guests(
    ll: *mut lowlat,
    out: *mut lowlat_guest,
    count: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            if count.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            let guests = seam.guests();
            let found = u32::try_from(guests.len()).unwrap_or(u32::MAX);
            if out.is_null() {
                count.write(found);
                return LOWLAT_OK;
            }
            let room = count.read() as usize;
            let writing = guests.len().min(room);
            for (at, guest) in guests.iter().take(writing).enumerate() {
                let mut slot = lowlat_guest {
                    number: guest.number,
                    permissions: lowlat_permissions {
                        keyboard: guest.permissions.keyboard,
                        pointer: guest.permissions.pointer,
                        gamepad: guest.permissions.gamepad,
                        reserved: 0,
                    },
                    owner: guest.owner,
                    reserved: [0; 3],
                    attempt: [0; LOWLAT_ATTEMPT_MAX],
                };
                put(&mut slot.attempt, &guest.attempt);
                out.add(at).write(slot);
            }
            if writing < guests.len() {
                count.write(found);
                return LOWLAT_ERR_TOO_SMALL;
            }
            count.write(u32::try_from(writing).unwrap_or(u32::MAX));
            LOWLAT_OK
        })
    }
}

/// Send one guest an application message, or every guest at once.
///
/// **Nothing here reads the body.** The sub-identifier and the bytes are an
/// agreement between an application and the clients it serves; a host that
/// interpreted either would be inventing a protocol on its behalf
/// ([05 §5](../../../docs/05-host.md)).
///
/// `guest_id` of [`LOWLAT_GUEST_ALL`] reaches everyone seated. A body past
/// what a peer will accept is refused here rather than sent and dropped in
/// silence at the far end.
///
/// # Safety
///
/// `data` must point to at least `len` bytes when `len` is not zero. It is
/// copied before the call returns and never retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_send_user_data(
    ll: *mut lowlat,
    guest_id: u32,
    id: u32,
    data: *const c_void,
    len: u32,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            if data.is_null() && len != 0 {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let body: &[u8] = if len == 0 {
                &[]
            } else {
                core::slice::from_raw_parts(data.cast::<u8>(), len as usize)
            };
            // Refused here, where the caller can still do something about it,
            // rather than at a far end that says nothing.
            if lowlat_core::control::string_body_len(body.len())
                > lowlat_core::control::USER_DATA_MAX
            {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            if guest_id == LOWLAT_GUEST_ALL {
                seam.send_user_data_all(id, body);
                // **Reaching nobody is not a failure.** An application
                // announcing something to an empty room has not made a
                // mistake, and reporting one would make it look like it had.
                return LOWLAT_OK;
            }
            if seam.send_user_data(guest_id, id, body) {
                LOWLAT_OK
            } else {
                LOWLAT_ERR_UNKNOWN_GUEST
            }
        })
    }
}

/// End one guest, telling it why.
///
/// **`reason` is not a [`lowlat_status`].** It reaches the peer as the
/// protocol's own disconnect status, which is a different numbering that
/// happens to share a width. **Zero is not a value to pass**: a peer carries on
/// through it, so a guest kicked with zero is told nothing and stays.
///
/// The guest is sent the reason, given a moment for it to arrive, and then its
/// seat goes back. It does not disappear from the roster the instant this
/// returns.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_kick_guest(
    ll: *mut lowlat,
    guest_id: u32,
    reason: i32,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            if reason == 0 {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            if seam.kick_guest(guest_id, reason) {
                LOWLAT_OK
            } else {
                LOWLAT_ERR_UNKNOWN_GUEST
            }
        })
    }
}

/// Change what one guest may drive, while it is connected.
///
/// **This is the only way to set them.** There is no separate call to turn a
/// guest's input off, because that is this call with every flag cleared, and
/// two calls writing one field can disagree about what a guest is allowed to
/// do.
///
/// The change reaches the roster immediately and the guest's own devices on its
/// next pass.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `perms` points to one
/// [`lowlat_permissions`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_set_permissions(
    ll: *mut lowlat,
    guest_id: u32,
    perms: *const lowlat_permissions,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(perms) = perms.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let permissions = lowlat_inject::event::Permissions {
                keyboard: perms.keyboard,
                pointer: perms.pointer,
                gamepad: perms.gamepad,
            };
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            if seam.set_permissions(guest_id, permissions) {
                LOWLAT_OK
            } else {
                LOWLAT_ERR_UNKNOWN_GUEST
            }
        })
    }
}

/// One output this host could be asked to capture.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_output {
    /// What to ask for, and what a capture-changed event reports. **Stable
    /// across a mode change**, which is why it is not the size.
    pub id: [c_char; LOWLAT_OUTPUT_MAX],
    /// The connector's own name, which is what the session knows it by and
    /// what a person recognises.
    pub name: [c_char; LOWLAT_OUTPUT_MAX],
    pub width: u32,
    pub height: u32,
    /// Where it sits in the desktop around it, which is the space absolute
    /// input is expressed against. Zero when no session said, which is also
    /// the corner: with one output the two are the same answer.
    pub x: u32,
    pub y: u32,
}

/// One sound output a host could capture.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
#[allow(non_camel_case_types)]
pub struct lowlat_audio_output {
    /// What to put in [`lowlat_host_audio_config::device`].
    ///
    /// **The monitor of the output, not the output**, because that is the
    /// device a host reads: it carries what the speakers are playing.
    pub id: [c_char; LOWLAT_OUTPUT_MAX],
    /// What a person calls it, which is the name to show them.
    pub name: [c_char; LOWLAT_OUTPUT_MAX],
}

/// List the sound outputs this host could capture.
///
/// **Available before hosting starts**, and it does not disturb a host that is
/// running: it asks over a connection of its own. Two calls and the caller's
/// own buffer, like the video one.
///
/// A machine with no sound server answers with none rather than failing, which
/// is the same thing an application does with it: offer what there is.
///
/// # Safety
///
/// `count` must be readable and writable, and `out`, when not null, must point
/// to at least `*count` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_get_audio_outputs(
    out: *mut lowlat_audio_output,
    count: *mut u32,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        if count.is_null() {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let listed = lowlat_audio::outputs(None).unwrap_or_default();
        let found = u32::try_from(listed.len()).unwrap_or(u32::MAX);
        if out.is_null() {
            unsafe { count.write(found) };
            return LOWLAT_OK;
        }
        let room = unsafe { count.read() } as usize;
        let writing = listed.len().min(room);
        for (at, output) in listed.iter().take(writing).enumerate() {
            let mut slot = lowlat_audio_output {
                id: [0; LOWLAT_OUTPUT_MAX],
                name: [0; LOWLAT_OUTPUT_MAX],
            };
            put(&mut slot.id, &output.id);
            put(&mut slot.name, &output.name);
            unsafe { out.add(at).write(slot) };
        }
        if writing < listed.len() {
            unsafe { count.write(found) };
            return LOWLAT_ERR_TOO_SMALL;
        }
        unsafe { count.write(u32::try_from(writing).unwrap_or(u32::MAX)) };
        LOWLAT_OK
    })
}

/// List the outputs this host could capture.
///
/// **Available before hosting starts**, so an application can present a choice
/// before committing to one. Two calls and the caller's own buffer, like the
/// roster: pass `NULL` to learn the count.
///
/// # Safety
///
/// `count` must be readable and writable, and `out`, when not null, must point
/// to at least `*count` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_get_outputs(
    out: *mut lowlat_output,
    count: *mut u32,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        if count.is_null() {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let listed = crate::display::Display::outputs();
        let found = u32::try_from(listed.len()).unwrap_or(u32::MAX);
        if out.is_null() {
            unsafe { count.write(found) };
            return LOWLAT_OK;
        }
        let room = unsafe { count.read() } as usize;
        let writing = listed.len().min(room);
        for (at, output) in listed.iter().take(writing).enumerate() {
            let mut slot = lowlat_output {
                id: [0; LOWLAT_OUTPUT_MAX],
                name: [0; LOWLAT_OUTPUT_MAX],
                width: output.width,
                height: output.height,
                x: 0,
                y: 0,
            };
            put(&mut slot.id, &output.id);
            put(&mut slot.name, &output.connector);
            if let Some(place) = output.place {
                slot.x = place.x;
                slot.y = place.y;
            }
            unsafe { out.add(at).write(slot) };
        }
        if writing < listed.len() {
            unsafe { count.write(found) };
            return LOWLAT_ERR_TOO_SMALL;
        }
        unsafe { count.write(u32::try_from(writing).unwrap_or(u32::MAX)) };
        LOWLAT_OK
    })
}

/// Whether this machine could host right now.
///
/// **A pre-flight, and the reason it exists is that the two ways of failing
/// look identical afterwards.** Starting a host that cannot capture fails deep
/// in the stream loop, where an application can tell "there is no display"
/// from "this process may not read one" only by reading a log. This answers
/// which, before anything is started.
///
/// [`LOWLAT_OK`] means a display is lit and its framebuffer can be reached.
/// It is a read: no encoder is built and no thread is started.
#[unsafe(no_mangle)]
pub extern "C" fn lowlat_can_host() -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        status_of(crate::display::Display::capturable())
    })
}

/// What each answer means at the boundary.
fn status_of(found: crate::display::Capturable) -> lowlat_status {
    match found {
        crate::display::Capturable::Yes => LOWLAT_OK,
        crate::display::Capturable::NothingLit => LOWLAT_ERR_NO_DISPLAY,
        crate::display::Capturable::NotReachable => LOWLAT_ERR_DISPLAY_UNREACHABLE,
    }
}

/// What a host is doing right now.
///
/// **What is happening, not what was asked for.** The picture's size is the
/// display's answer and the guest count is the room's; the settings that
/// produced them are read back through [`lowlat_host_get_video_config`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_host_status {
    /// Set by the caller to `sizeof(lowlat_host_status)`.
    pub size: u32,
    /// Guests that are connected and addressable.
    pub guests: u32,
    /// The picture the stream is producing. **Zero before a display has been
    /// opened**, which is the honest answer: until then the size is the
    /// display's to decide and nothing here knows it.
    pub width: u32,
    pub height: u32,
    /// Whether this handle is hosting.
    pub running: bool,
    /// Whether a sound device is being read right now.
    ///
    /// **Not what sound is set to.** Nothing is read while nobody is
    /// listening, so this is clear in an empty room however sound is
    /// configured; and it is also clear when the device could not be opened or
    /// has gone away, which is the case an application cannot learn any other
    /// way -- the settings still say enabled, because they are what was asked
    /// for.
    pub audio_active: bool,
    pub reserved: [u8; 2],
    /// The sound device being read, empty when none is.
    ///
    /// **What it landed on, not what was asked for.** An empty request means
    /// the default output's monitor, and the sound server can move a stream
    /// while it runs, so this is the only place the two can be compared.
    pub audio_device: [c_char; LOWLAT_OUTPUT_MAX],
}

/// Read what the host is doing.
///
/// Answers on a handle that is not hosting too, with `running` clear: an
/// application asking what state something is in should not have to know the
/// answer first.
///
/// # Safety
///
/// `out` points to one [`lowlat_host_status`] whose `size` says how much of it
/// is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_get_status(
    ll: *mut lowlat,
    out: *mut lowlat_host_status,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(slot) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (slot.size as usize) < core::mem::size_of::<lowlat_host_status>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                slot.guests = 0;
                slot.width = 0;
                slot.height = 0;
                slot.running = false;
                slot.audio_active = false;
                slot.reserved = [0; 2];
                put(&mut slot.audio_device, "");
                return LOWLAT_OK;
            };
            let (width, height) = seam.picture().unwrap_or((0, 0));
            let (reading, device) = seam.audio_state();
            slot.guests = u32::try_from(seam.guests().len()).unwrap_or(u32::MAX);
            slot.width = width;
            slot.height = height;
            slot.running = true;
            slot.audio_active = reading;
            slot.reserved = [0; 2];
            put(
                &mut slot.audio_device,
                device.as_deref().unwrap_or_default(),
            );
            LOWLAT_OK
        })
    }
}

/// Tell every guest who is in the room.
///
/// **A different message from an application message, and not a variant of
/// one.** It travels on its own opcode, it is addressed to everybody rather
/// than to a guest, and each peer finds *itself* in the list by number and
/// takes that entry as what it is allowed to do. A peer has no way to ask for
/// it, so one that is never sent one does not know what it is.
///
/// **The body's shape belongs to the clients an application serves**, exactly
/// as an application message's does; nothing here reads it.
///
/// Answers how many guests it reached, which is zero for an empty room and not
/// an error.
///
/// # Safety
///
/// `data` must point to at least `len` bytes when `len` is not zero. It is
/// copied before the call returns and never retained.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_send_roster(
    ll: *mut lowlat,
    data: *const c_void,
    len: u32,
    reached: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            if data.is_null() && len != 0 {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let body: &[u8] = if len == 0 {
                &[]
            } else {
                core::slice::from_raw_parts(data.cast::<u8>(), len as usize)
            };
            if lowlat_core::control::string_body_len(body.len())
                > lowlat_core::control::USER_DATA_MAX
            {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let mut held = handle.held();
            let Some(seam) = held.seam.as_mut() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            let sent = seam.send_roster(body);
            if let Some(slot) = reached.as_mut() {
                *slot = u32::try_from(sent).unwrap_or(u32::MAX);
            }
            LOWLAT_OK
        })
    }
}

/// What one guest is doing.
///
/// **Its own structure behind its own call, and that is deliberate.** A guest
/// is delivered as an array element and an array element cannot carry a `size`
/// -- the caller walks it by stride -- so [`lowlat_guest`] is fixed for the
/// major version. These are the numbers most likely to grow, so they live
/// where growing them is free.
///
/// **One stream, not three.** This host produces one and switches which
/// display feeds it, so there is nothing to index.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_metrics {
    /// Set by the caller to `sizeof(lowlat_metrics)`.
    pub size: u32,
    /// How long this guest has been connected.
    pub connected_ms: u32,
    /// When each kind of input last arrived, on the same clock as
    /// `connected_ms`. **Zero means never**, which is not zero milliseconds
    /// ago -- an application kicking idle guests has to tell the two apart.
    pub keyboard_ms: u32,
    pub pointer_ms: u32,
    pub gamepad_ms: u32,
    /// Video frames sent to this guest.
    pub frames: u32,
    /// Fragments outstanding, and how many are past due. **These are the
    /// controller's own inputs**, so an application reads what the host is
    /// steering by rather than a second set derived elsewhere; together they
    /// are what "chronically behind" means.
    pub window: u32,
    pub stale: u32,
    /// Times congestion cost this guest rate.
    pub cg_events: u32,
    pub bitrate_mbps: f32,
    pub encode_ms: f32,
    /// The smoothed round trip to this peer.
    pub network_ms: f32,
}

/// Read what one guest is doing.
///
/// **What this host can answer for, and nothing else.** A peer's own decode
/// time and how many frames it has queued waiting to decode are the peer's to
/// know; reporting either would be reporting a number this host made up.
///
/// # Safety
///
/// `out` points to one [`lowlat_metrics`] whose `size` says how much of it is
/// set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_get_metrics(
    ll: *mut lowlat,
    guest_id: u32,
    out: *mut lowlat_metrics,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(slot) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (slot.size as usize) < core::mem::size_of::<lowlat_metrics>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            let Some(found) = seam
                .guests()
                .into_iter()
                .find(|guest| guest.number == guest_id)
            else {
                return LOWLAT_ERR_UNKNOWN_GUEST;
            };
            let metrics = found.metrics;
            slot.connected_ms = metrics.connected_ms;
            slot.keyboard_ms = metrics.keyboard_ms;
            slot.pointer_ms = metrics.pointer_ms;
            slot.gamepad_ms = metrics.gamepad_ms;
            slot.frames = metrics.frames;
            slot.window = metrics.window;
            slot.stale = metrics.stale;
            slot.cg_events = metrics.cg_events;
            slot.bitrate_mbps = metrics.bitrate_mbps;
            slot.encode_ms = metrics.encode_ms;
            slot.network_ms = metrics.network_ms;
            LOWLAT_OK
        })
    }
}

/// Change the video settings while the host runs.
///
/// **Everything in this structure is applied without rebuilding the session.**
/// The bitrate re-bases the budget and reaches the encoder through the
/// reconfigure the rate loop already does, so it costs no keyframe and no
/// interruption; the frame rate changes the pacing from the next frame. The
/// output is the exception in cost rather than in kind: a different picture
/// cannot be absorbed into a stream built for another one, so it rebuilds
/// around the new source and costs one coded refresh, keeping every guest on
/// its seat and its channel.
///
/// Refused with [`LOWLAT_ERR_INVALID_ARGUMENT`] when the host is not running,
/// because there is nothing yet for the values to apply to and accepting them
/// silently would report settings that never took.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `cfg` points to one
/// [`lowlat_host_video_config`] whose `size` says how much of it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_set_video_config(
    ll: *mut lowlat,
    cfg: *const lowlat_host_video_config,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(cfg) = cfg.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let Some(video) = video_configured(cfg) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let Ok(output) = taken(&cfg.output).ok_or(()) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            seam.set_video(video);
            seam.select_output((!output.is_empty()).then(|| output.to_string()));
            LOWLAT_OK
        })
    }
}

/// Change what sound is set to, while a host runs.
///
/// **Every field takes effect without a restart.** Switching sound off gives
/// the device back and restores the speakers; switching it on takes it again.
/// A device that does not resolve is refused rather than substituted, and the
/// host keeps the one it has.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `cfg` points to one
/// [`lowlat_host_audio_config`] whose `size` says how much of it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_set_audio_config(
    ll: *mut lowlat,
    cfg: *const lowlat_host_audio_config,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(cfg) = cfg.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let Some((on, allow_raw, kbps, live)) = audio_configured(cfg) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            // **A named device is checked here rather than where it is
            // opened.** A sound server substitutes something plausible for a
            // name it does not know, so a caller told yes would be capturing
            // something it did not ask for; and the loop that opens it cannot
            // refuse a call that has already returned. Checked before the
            // handle is locked, because it asks the server over a connection
            // of its own.
            if let Some(device) = live.device.as_deref()
                && !lowlat_audio::outputs(None)
                    .is_ok_and(|listed| listed.iter().any(|output| output.id == device))
            {
                lowlat_common::log_warn!("abi: no such sound device, sound is unchanged");
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            seam.set_audio(on, allow_raw, kbps, &live);
            LOWLAT_OK
        })
    }
}

/// What sound is set to now.
///
/// **Read back rather than remembered**, for the reason the video one is: what
/// a host is set to is the host's answer and another caller may have changed
/// it.
///
/// **These are the settings and not the state.** `device` is the request, so
/// an application that reads this, changes one field and writes it back does
/// not accidentally pin a host that was following the default output. What is
/// actually being read, and whether anything is, is in
/// [`lowlat_host_status`].
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `out` points to one
/// [`lowlat_host_audio_config`] whose `size` says how much of it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_get_audio_config(
    ll: *mut lowlat,
    out: *mut lowlat_host_audio_config,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(slot) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (slot.size as usize) < core::mem::size_of::<lowlat_host_audio_config>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let (on, allow_raw, kbps, live) = seam.audio();
            slot.enabled = on;
            slot.allow_uncompressed = allow_raw;
            slot.bitrate_kbps = kbps;
            slot.mute_local = live.mute_local;
            slot.reserved = [0; 1];
            put(&mut slot.device, live.device.as_deref().unwrap_or_default());
            LOWLAT_OK
        })
    }
}

/// What the host is running at now.
///
/// **Read back rather than remembered.** What a stream is doing is the
/// stream's answer, and an application that kept its own copy would be
/// describing settings another guest may have changed underneath it.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`], and `out` points to one
/// [`lowlat_host_video_config`] whose `size` says how much of it is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_get_video_config(
    ll: *mut lowlat,
    out: *mut lowlat_host_video_config,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            let Some(slot) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (slot.size as usize) < core::mem::size_of::<lowlat_host_video_config>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let Some(seam) = held.seam.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let Some(video) = seam.video() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            slot.fps = video.fps;
            slot.bitrate_mbps = video.bitrate_mbps;
            slot.min_bitrate_mbps = video.min_mbps;
            slot.full_fps = video.full_fps;
            slot.reserved = [0; 3];
            // **What is being captured, not what was asked for.** A guest can
            // switch outputs and a display can move by itself; an application
            // told the request marks the wrong screen.
            let listed = crate::display::Display::outputs();
            let running = crate::display::captured(&listed, seam.captured())
                .map(|output| output.id.clone())
                .unwrap_or_default();
            put(&mut slot.output, &running);
            LOWLAT_OK
        })
    }
}

/// Stop hosting, disconnecting every guest and joining every thread.
///
/// **Not the same as destroying the handle.** A host may be stopped and started
/// again on the same handle, and events raised before it stopped are still
/// waiting to be polled.
///
/// **A peer is not yet told why.** Guest loops are stopped and joined, and the
/// far side learns by its own liveness deadline rather than from a message, so
/// stopping costs a peer the wait rather than being immediate to it. There is
/// no reason parameter here because there is nothing yet that could carry one.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_stop(ll: *mut lowlat) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            // The queue outlives the seam on purpose: what it raised on the way
            // down is still worth polling.
            handle.held().seam = None;
            LOWLAT_OK
        })
    }
}

/// Take one event, waiting up to `timeout_ms` for one to arrive.
///
/// Answers [`LOWLAT_TIMEOUT`] when nothing arrived, which is not an error. A
/// `timeout_ms` of zero polls without waiting.
///
/// `body` receives an application message's body and may be null, which means
/// the application does not want bodies: one that arrives is delivered without
/// it, and the event still says how long it was. When `body` is not null,
/// `body_len` carries its capacity in and the bytes written out.
///
/// **A body that does not fit consumes nothing.** [`LOWLAT_ERR_TOO_SMALL`] is
/// answered, `body_len` is set to what the body needs, and the same event is
/// delivered by the next call with room for it.
///
/// # Safety
///
/// `out` must point to one [`lowlat_event`]. `body`, when not null, must point
/// to at least `*body_len` bytes, and `body_len` must then be readable and
/// writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_host_poll_events(
    ll: *mut lowlat,
    timeout_ms: u32,
    out: *mut lowlat_event,
    body: *mut c_void,
    body_len: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(ll, |handle| {
            if out.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            if !body.is_null() && body_len.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let timeout = Duration::from_millis(u64::from(timeout_ms));
            let events = &handle.events;

            if body.is_null() {
                // The body is dropped, and the event still reports its length
                // so that what was given up is visible.
                let Some(received) = events.recv_timeout(timeout) else {
                    return LOWLAT_TIMEOUT;
                };
                out.write(described(&received));
                return LOWLAT_OK;
            }

            let capacity = body_len.read() as usize;
            let buffer = core::slice::from_raw_parts_mut(body.cast::<u8>(), capacity);
            match events.recv_timeout_into(timeout, buffer) {
                Delivery::Empty => LOWLAT_TIMEOUT,
                Delivery::TooSmall { needed } => {
                    body_len.write(u32::try_from(needed).unwrap_or(u32::MAX));
                    LOWLAT_ERR_TOO_SMALL
                }
                Delivery::Took(received) => {
                    let event = described(&received);
                    let written = if event.kind == LOWLAT_EVENT_USER_DATA {
                        event.body.user_data.body_len
                    } else {
                        0
                    };
                    body_len.write(written);
                    out.write(event);
                    LOWLAT_OK
                }
            }
        })
    }
}

/// Panic on purpose, and prove the boundary contains it.
///
/// **Exported by the shipped library rather than hidden behind a build
/// option**, because what has to be tested is that *this* object still
/// unwinds. Building it to abort on panic silently disables containment
/// everywhere, and the same code linked into a test binary answers for the
/// test's build rather than for this one.
/// **It takes the handle** so that what follows a contained panic is testable
/// too: the handle is poisoned, every later call on it is refused, and
/// destroying it still works.
///
/// # Safety
///
/// `ll` came from [`lowlat_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_debug_panic(ll: *mut lowlat) -> lowlat_status {
    unsafe {
        entered(ll, |_| {
            panic!("deliberate panic, to prove the boundary contains one")
        })
    }
}

/// Read a caller's NUL-terminated string.
///
/// **Bounded rather than trusted.** A pointer with no terminator inside a
/// sane length is a caller that handed over something that is not a string,
/// and walking it to find out is how a library reads somebody else's memory.
unsafe fn read_c_str<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(text.cast(), LOWLAT_ATTEMPT_MAX) };
    let end = bytes.iter().position(|byte| *byte == 0)?;
    core::str::from_utf8(bytes.get(..end)?).ok()
}

/// Copy text into a fixed array, terminated, truncating if it must.
fn put(into: &mut [c_char], text: &str) {
    into.fill(0);
    let room = into.len().saturating_sub(1);
    for (slot, byte) in into.iter_mut().zip(text.as_bytes().iter().take(room)) {
        *slot = *byte as c_char;
    }
}

/// Say the address and port of a socket into an event's fields.
fn put_address(into: &mut [c_char], port: &mut u16, addr: &std::net::SocketAddr) {
    put(into, &addr.ip().to_string());
    *port = addr.port();
}

/// Describe one event in the shape the boundary publishes.
fn described(received: &crate::events::Received) -> lowlat_event {
    let dropped = received.dropped;
    match &received.event {
        Event::Candidate {
            attempt,
            addr,
            from_stun,
        } => {
            let mut body = lowlat_candidate_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
                address: [0; LOWLAT_ADDRESS_MAX],
                port: 0,
                from_stun: *from_stun,
                reserved: 0,
            };
            put(&mut body.attempt, attempt);
            put_address(&mut body.address, &mut body.port, addr);
            lowlat_event {
                kind: LOWLAT_EVENT_CANDIDATE,
                dropped,
                body: lowlat_event_body { candidate: body },
            }
        }
        Event::Ready { attempt } => {
            let mut body = lowlat_ready_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
            };
            put(&mut body.attempt, attempt);
            lowlat_event {
                kind: LOWLAT_EVENT_READY,
                dropped,
                body: lowlat_event_body { ready: body },
            }
        }
        Event::Established { attempt, addr } => {
            let mut body = lowlat_established_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
                address: [0; LOWLAT_ADDRESS_MAX],
                port: 0,
                reserved: [0; 2],
            };
            put(&mut body.attempt, attempt);
            put_address(&mut body.address, &mut body.port, addr);
            lowlat_event {
                kind: LOWLAT_EVENT_ESTABLISHED,
                dropped,
                body: lowlat_event_body { established: body },
            }
        }
        Event::Ended { attempt, outcome } => {
            // Exhaustive on purpose: a new way for an attempt to finish should
            // break this build rather than reach an application as a number it
            // has no name for.
            let (outcome, reason) = match outcome {
                Outcome::ConnectivityFailed => (LOWLAT_OUTCOME_CONNECTIVITY_FAILED, 0),
                Outcome::PeerGone => (LOWLAT_OUTCOME_PEER_GONE, 0),
                Outcome::Undeliverable => (LOWLAT_OUTCOME_UNDELIVERABLE, 0),
                Outcome::PeerLeft => (LOWLAT_OUTCOME_PEER_LEFT, 0),
                Outcome::NeverDeclared => (LOWLAT_OUTCOME_NEVER_DECLARED, 0),
                Outcome::TransportFailed => (LOWLAT_OUTCOME_TRANSPORT_FAILED, 0),
                Outcome::ControlStalled => (LOWLAT_OUTCOME_CONTROL_STALLED, 0),
                Outcome::Kicked(reason) => (LOWLAT_OUTCOME_KICKED, *reason),
            };
            let mut body = lowlat_ended_event {
                attempt: [0; LOWLAT_ATTEMPT_MAX],
                outcome,
                reason,
            };
            put(&mut body.attempt, attempt);
            lowlat_event {
                kind: LOWLAT_EVENT_ENDED,
                dropped,
                body: lowlat_event_body { ended: body },
            }
        }
        Event::CaptureChanged {
            width,
            height,
            output,
        } => {
            let mut body = lowlat_capture_changed_event {
                width: *width,
                height: *height,
                output: [0; LOWLAT_OUTPUT_MAX],
            };
            put(&mut body.output, output);
            lowlat_event {
                kind: LOWLAT_EVENT_CAPTURE_CHANGED,
                dropped,
                body: lowlat_event_body {
                    capture_changed: body,
                },
            }
        }
        Event::InputOwnerChanged { guest } => lowlat_event {
            kind: LOWLAT_EVENT_INPUT_OWNER_CHANGED,
            dropped,
            body: lowlat_event_body {
                input_owner: lowlat_input_owner_event {
                    guest: guest.unwrap_or(LOWLAT_GUEST_ALL),
                },
            },
        },
        Event::Fatal { reason } => lowlat_event {
            kind: LOWLAT_EVENT_FATAL,
            dropped,
            body: lowlat_event_body {
                fatal: lowlat_fatal_event { reason: *reason },
            },
        },
        Event::UserData { guest, id, text } => lowlat_event {
            kind: LOWLAT_EVENT_USER_DATA,
            dropped,
            body: lowlat_event_body {
                user_data: lowlat_user_data_event {
                    guest: *guest,
                    id: *id,
                    body_len: u32::try_from(text.len()).unwrap_or(u32::MAX),
                },
            },
        },
    }
}

/// Run an entry point that needs the handle.
///
/// Null is refused, a poisoned handle is refused, and a panic poisons it. **The
/// poisoning is what makes asserting unwind safety sound**: state a panic may
/// have left half-written is never read again, because every later call stops
/// here.
///
/// # Safety
///
/// `ll` is null or came from [`lowlat_create`].
unsafe fn entered(ll: *mut lowlat, call: impl FnOnce(&lowlat) -> lowlat_status) -> lowlat_status {
    let Some(handle) = (unsafe { ll.as_ref() }) else {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    };
    if handle.poisoned.load(Ordering::Acquire) {
        return LOWLAT_ERR_POISONED;
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| call(handle))) {
        Ok(status) => status,
        Err(_) => {
            handle.poisoned.store(true, Ordering::Release);
            lowlat_common::log_error!("abi: a call panicked, contained and the handle poisoned");
            LOWLAT_ERR_INTERNAL
        }
    }
}

/// Run one entry point's body with unwinding contained.
///
/// A panic crossing an `extern "C"` boundary is undefined behaviour and this
/// library loads into processes we do not control, so every entry point that
/// runs any of our code goes through here.
///
/// **Unwind safety is asserted rather than proven**, and what makes that sound
/// is the poisoning that arrives with the handle: state a panic may have left
/// half-written is never read again, because every later call on that handle
/// is refused before it reaches this point.
fn guard<T>(contained: T, call: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(call)) {
        Ok(value) => value,
        Err(_) => {
            lowlat_common::log_error!("abi: a call panicked, contained at the boundary");
            contained
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The version is packed, not added.** A minor of 1 and a major of 1 are
    /// different versions, and a loader that compares a sum accepts one for
    /// the other.
    #[test]
    fn the_version_packs_major_above_minor() {
        let packed = lowlat_abi_version();
        assert_eq!(packed >> 16, LOWLAT_ABI_MAJOR);
        assert_eq!(packed & 0xffff, LOWLAT_ABI_MINOR);
    }

    /// **Every status describes itself, and an undefined one still answers.**
    /// A caller reaches for this while something is already wrong, so a null
    /// pointer here costs the diagnosis it was called for.
    #[test]
    fn every_status_describes_itself_and_so_does_one_we_never_defined() {
        for status in [
            LOWLAT_OK,
            LOWLAT_TIMEOUT,
            LOWLAT_ERR_INTERNAL,
            LOWLAT_ERR_INVALID_ARGUMENT,
            LOWLAT_ERR_TOO_SMALL,
            LOWLAT_ERR_POISONED,
            LOWLAT_ERR_ALREADY_STARTED,
            LOWLAT_ERR_NOT_STARTED,
            LOWLAT_ERR_AT_CAPACITY,
            LOWLAT_ERR_UNKNOWN_ATTEMPT,
            LOWLAT_ERR_ALREADY_BEGUN,
            LOWLAT_ERR_WITHDRAWN,
            LOWLAT_ERR_IO,
            LOWLAT_ERR_CRYPTO,
            LOWLAT_ERR_UNKNOWN_GUEST,
            LOWLAT_ERR_NO_DISPLAY,
            LOWLAT_ERR_DISPLAY_UNREACHABLE,
        ] {
            let text = lowlat_status_string(status as i32);
            assert!(!text.is_null());
            // Safe: the pointer is to a literal with static storage.
            let text = unsafe { CStr::from_ptr(text) };
            assert_ne!(
                text.to_bytes(),
                b"unknown status",
                "{status:?} is missing from the description table"
            );
        }
        let unknown = unsafe { CStr::from_ptr(lowlat_status_string(-31337)) };
        assert_eq!(unknown.to_bytes(), b"unknown status");
    }

    /// **A panic is contained, and what follows it is refused.** The test
    /// that matters runs against the built shared object, in `tests/abi.rs`;
    /// this one says the handle is poisoned by it and that destroying a
    /// poisoned handle still works, which is the half a C harness cannot see.
    #[test]
    fn a_panic_poisons_the_handle_and_destroying_it_still_works() {
        let mut handle: *mut lowlat = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        assert!(!handle.is_null());

        assert_eq!(unsafe { lowlat_debug_panic(handle) }, LOWLAT_ERR_INTERNAL);
        // Every later call, including another panic, stops at the poison.
        assert_eq!(unsafe { lowlat_debug_panic(handle) }, LOWLAT_ERR_POISONED);
        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        assert_eq!(
            unsafe {
                lowlat_host_poll_events(
                    handle,
                    0,
                    event.as_mut_ptr(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            LOWLAT_ERR_POISONED
        );

        unsafe { lowlat_destroy(handle) };
    }

    /// **A null handle is refused rather than dereferenced**, which is the
    /// first thing any application does by accident.
    #[test]
    fn a_null_handle_is_refused() {
        assert_eq!(
            unsafe { lowlat_debug_panic(core::ptr::null_mut()) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        // And destroying nothing is allowed, so an application's cleanup path
        // needs no branch of its own.
        unsafe { lowlat_destroy(core::ptr::null_mut()) };
    }

    /// **Polling a host that has not started waits on a real queue.** An
    /// application starts its polling thread before it starts hosting, so a
    /// poll that answered instantly would make that thread a spin -- and the
    /// queue exists from creation rather than arriving with a host, so there
    /// is nothing to special-case.
    #[test]
    fn polling_before_hosting_waits_and_then_times_out() {
        let mut handle: *mut lowlat = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        let began = lowlat_common::clock::Time::now();
        assert_eq!(
            unsafe {
                lowlat_host_poll_events(
                    handle,
                    60,
                    event.as_mut_ptr(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            LOWLAT_TIMEOUT
        );
        let waited = lowlat_common::clock::elapsed_ms(began);
        assert!(waited >= 50.0, "returned after {waited:.1} ms, so it spun");
        unsafe { lowlat_destroy(handle) };
    }
}

#[cfg(test)]
mod start_tests {
    use super::*;

    pub(super) fn video() -> lowlat_host_video_config {
        lowlat_host_video_config {
            size: u32::try_from(core::mem::size_of::<lowlat_host_video_config>())
                .unwrap_or(u32::MAX),
            fps: 60,
            bitrate_mbps: 10.0,
            min_bitrate_mbps: 1.0,
            full_fps: true,
            reserved: [0; 3],
            output: [0; LOWLAT_OUTPUT_MAX],
        }
    }

    pub(super) fn config() -> lowlat_host_config {
        lowlat_host_config {
            size: u32::try_from(core::mem::size_of::<lowlat_host_config>()).unwrap_or(u32::MAX),
            base_port: 9000,
            reserved: 0,
            max_guests: 4,
            codec: lowlat_codec::LOWLAT_CODEC_H264 as u32,
            encoder: lowlat_encoder::LOWLAT_ENCODER_FOLLOW_DISPLAY as u32,
            cg_level: lowlat_cg_level::LOWLAT_CG_LEVEL_SENSITIVE as u32,
            exclusive_hold_ms: 500,
            exclusive_pointer: false,
            reserved2: [0; 3],
            server_count: 0,
            servers: [[0; LOWLAT_SERVER_MAX]; LOWLAT_SERVERS_MAX],
            video: video(),
            audio: audio(),
        }
    }

    pub(super) fn audio() -> lowlat_host_audio_config {
        lowlat_host_audio_config {
            size: u32::try_from(core::mem::size_of::<lowlat_host_audio_config>())
                .unwrap_or(u32::MAX),
            bitrate_kbps: 128,
            // **Off in the fixtures**, because a test that opened a sound
            // device would be a test of the machine it runs on.
            enabled: false,
            allow_uncompressed: false,
            mute_local: false,
            reserved: [0; 1],
            device: [0; LOWLAT_OUTPUT_MAX],
        }
    }

    pub(super) fn handle() -> *mut lowlat {
        let mut handle: *mut lowlat = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        handle
    }

    /// Hosting starts, stops, and starts again on the same handle.
    #[test]
    fn a_host_starts_and_stops_and_starts_again() {
        let handle = handle();
        let cfg = config();
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );
        // **Starting twice is refused rather than silently reconfiguring.** A
        // second configuration that looks accepted and is not is a host running
        // settings nobody can see.
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_ERR_ALREADY_STARTED
        );
        assert_eq!(unsafe { lowlat_host_stop(handle) }, LOWLAT_OK);
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );
        unsafe { lowlat_destroy(handle) };
    }

    /// **What a caller sets at start is what it reads back.** A field the
    /// boundary accepts and then drops is worse than one it refuses: the
    /// application believes it asked for something. This one was dropped --
    /// the stream's configuration had no such field, so the live cell was
    /// built from a default and the start-time value went nowhere.
    #[test]
    fn a_setting_made_at_start_is_the_one_read_back() {
        let handle = handle();
        let mut cfg = config();
        cfg.video.full_fps = true;
        cfg.video.fps = 45;
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );

        let mut back = video();
        assert_eq!(
            unsafe { lowlat_host_get_video_config(handle, &raw mut back) },
            LOWLAT_OK
        );
        assert!(back.full_fps, "a permission set at start was dropped");
        assert_eq!(back.fps, 45);
        unsafe { lowlat_destroy(handle) };
    }

    /// **Every enumerated field is checked, not transmuted.** The application
    /// fills this structure, so each of these is whatever it wrote, and a value
    /// nothing defined must be refused rather than become a variant that does
    /// not exist.
    #[test]
    fn a_value_nothing_defines_is_refused() {
        let handle = handle();
        for spoil in [
            (|cfg: &mut lowlat_host_config| cfg.codec = 99) as fn(&mut lowlat_host_config),
            |cfg| cfg.encoder = 99,
            |cfg| cfg.cg_level = 99,
            |cfg| cfg.max_guests = 0,
            |cfg| cfg.max_guests = LOWLAT_GUESTS_MAX + 1,
            |cfg| cfg.video.fps = 0,
            |cfg| cfg.video.bitrate_mbps = 0.0,
            |cfg| cfg.video.bitrate_mbps = f64::NAN,
            // A floor above the ceiling leaves congestion control nowhere to go.
            |cfg| cfg.video.min_bitrate_mbps = 100.0,
            |cfg| cfg.server_count = u32::try_from(LOWLAT_SERVERS_MAX + 1).unwrap_or(u32::MAX),
            // A field with no terminator was overrun by whoever filled it.
            |cfg| cfg.video.output = [b'x' as c_char; LOWLAT_OUTPUT_MAX],
            // The size field is the versioning, and one that says less than the
            // struct holds means the caller and the header disagree.
            |cfg| cfg.size = 4,
            |cfg| cfg.video.size = 4,
        ] {
            let mut cfg = config();
            spoil(&mut cfg);
            assert_eq!(
                unsafe { lowlat_host_start(handle, &raw const cfg) },
                LOWLAT_ERR_INVALID_ARGUMENT,
                "a configuration that should have been refused was accepted"
            );
        }
        // And none of that left it hosting.
        let good = config();
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const good) },
            LOWLAT_OK
        );
        unsafe { lowlat_destroy(handle) };
    }

    /// **Both families of a dual-stack reflexive server are asked.**
    ///
    /// A name that answers with A and AAAA is ordered by this machine's own
    /// addressing, so keeping the head alone gives a host with global v6 a v6
    /// reflexive address and no v4 one, and a v4-only peer is then offered
    /// nothing from us it can reach. Asserted per family rather than on order,
    /// which belongs to the resolver.
    #[test]
    fn a_dual_stack_reflexive_server_contributes_both_families() {
        // Stated rather than assumed: on a machine whose localhost answers on
        // one family only, this test cannot tell the two behaviours apart, and
        // saying so is better than passing without having checked.
        let families: std::collections::BTreeSet<bool> =
            std::net::ToSocketAddrs::to_socket_addrs("localhost:3478")
                .expect("localhost must resolve")
                .map(|addr| addr.is_ipv6())
                .collect();
        assert_eq!(
            families.len(),
            2,
            "this check needs a localhost that answers on both families"
        );

        let mut cfg = config();
        cfg.server_count = 1;
        put(&mut cfg.servers[0], "localhost:3478");

        let translated = configured(&cfg).expect("a resolvable server is configuration");
        assert_eq!(
            translated.servers.iter().filter(|a| a.is_ipv4()).count(),
            1,
            "no v4 reflexive server came out of a dual-stack name: {:?}",
            translated.servers
        );
        assert_eq!(
            translated.servers.iter().filter(|a| a.is_ipv6()).count(),
            1,
            "no v6 reflexive server came out of a dual-stack name: {:?}",
            translated.servers
        );
    }

    /// A reflexive server is resolved while the caller is still holding the
    /// call that set it, rather than failing later where nothing can be done.
    #[test]
    fn a_server_that_does_not_resolve_is_refused_at_the_call_that_set_it() {
        let handle = handle();
        let mut cfg = config();
        cfg.server_count = 1;
        put(&mut cfg.servers[0], "no-such-host.invalid:3478");
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );

        let mut cfg = config();
        cfg.server_count = 1;
        put(&mut cfg.servers[0], "127.0.0.1:3478");
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );
        unsafe { lowlat_destroy(handle) };
    }

    /// **The queue outlives the host on it.** It exists from creation, so a
    /// poll before hosting waits on something real; and stopping does not empty
    /// it, because what was raised on the way down is still worth taking.
    #[test]
    fn events_reach_the_poll_once_hosting_has_started() {
        let handle = handle();
        let cfg = config();
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );

        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        let polled = unsafe {
            lowlat_host_poll_events(
                handle,
                0,
                event.as_mut_ptr(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        // Nobody has offered, so there is nothing yet -- but the queue is real
        // now rather than absent, which is what the next assertion rests on.
        assert_eq!(polled, LOWLAT_TIMEOUT);

        // Raised by the seam and still takeable after the host that raised it
        // has gone.
        assert_eq!(unsafe { lowlat_host_stop(handle) }, LOWLAT_OK);
        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        let after = unsafe {
            lowlat_host_poll_events(
                handle,
                0,
                event.as_mut_ptr(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert!(
            after == LOWLAT_OK || after == LOWLAT_TIMEOUT,
            "the queue went away with the host that was raising into it"
        );
        unsafe { lowlat_destroy(handle) };
    }
}

#[cfg(test)]
mod seam_tests {
    use super::start_tests::{config, handle};
    use super::*;

    fn attempt(id: &str) -> lowlat_attempt_info {
        let mut info = lowlat_attempt_info {
            size: u32::try_from(core::mem::size_of::<lowlat_attempt_info>()).unwrap_or(u32::MAX),
            reserved: 0,
            attempt_id: [0; LOWLAT_ATTEMPT_MAX],
            ufrag: [0; LOWLAT_ICE_MAX],
            pwd: [0; LOWLAT_ICE_MAX],
            aes256: [0; LOWLAT_ICE_MAX],
            permissions: lowlat_permissions {
                keyboard: true,
                pointer: true,
                gamepad: true,
                reserved: 0,
            },
            owner: false,
            reserved2: [0; 3],
        };
        put(&mut info.attempt_id, id);
        put(&mut info.ufrag, "G+sZxQ==");
        put(&mut info.pwd, "Det3D+arYViymh6I2v7UaOnrsHieoTRE");
        info
    }

    /// Register and approve one attempt, which is what gives it a number.
    pub(super) fn approved(handle: *mut lowlat, id: &str) {
        let info = attempt(id);
        assert_eq!(
            unsafe { lowlat_host_new_attempt(handle, &raw const info) },
            LOWLAT_OK
        );
        let mut ours = credentials();
        let id = std::ffi::CString::new(id).expect("no interior nul");
        assert_eq!(
            unsafe { lowlat_host_begin_p2p(handle, id.as_ptr(), &raw mut ours) },
            LOWLAT_OK
        );
    }

    pub(super) fn started() -> *mut lowlat {
        let handle = handle();
        let cfg = config();
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );
        handle
    }

    fn credentials() -> lowlat_credentials {
        lowlat_credentials {
            size: u32::try_from(core::mem::size_of::<lowlat_credentials>()).unwrap_or(u32::MAX),
            port: 0,
            reserved: 0,
            ufrag: [0; LOWLAT_ICE_MAX],
            pwd: [0; LOWLAT_ICE_MAX],
            fingerprint: [0; LOWLAT_FINGERPRINT_MAX],
            aes256: [0; LOWLAT_ICE_MAX],
        }
    }

    /// The four calls, in the order an application makes them.
    #[test]
    fn an_attempt_is_registered_approved_and_ended() {
        let handle = started();
        let info = attempt("a");
        assert_eq!(
            unsafe { lowlat_host_new_attempt(handle, &raw const info) },
            LOWLAT_OK
        );

        let mut ours = credentials();
        assert_eq!(
            unsafe { lowlat_host_begin_p2p(handle, c"a".as_ptr(), &raw mut ours) },
            LOWLAT_OK
        );
        // **The port that was bound, not the one that was configured.** The
        // bind walks when a port is taken, and advertising the configured one
        // produces a peer that answers checks and never establishes.
        assert_ne!(ours.port, 0);
        for field in [&ours.ufrag[..], &ours.pwd[..], &ours.fingerprint[..]] {
            assert!(
                taken(field).is_some_and(|text| !text.is_empty()),
                "a credential came back empty"
            );
        }
        assert_eq!(
            taken(&ours.aes256).map(str::len),
            Some(254),
            "the media key is the field this array is sized for"
        );

        unsafe { lowlat_host_end_connection(handle, c"a".as_ptr()) };
        unsafe { lowlat_destroy(handle) };
    }

    /// **Every refusal is its own status.** An application declines an offer
    /// for a full host and retries a race with teardown; collapsing them into
    /// one failure leaves it unable to tell which it is looking at.
    #[test]
    fn each_way_of_refusing_an_attempt_has_its_own_status() {
        let handle = started();

        // Approving something never registered.
        let mut ours = credentials();
        assert_eq!(
            unsafe { lowlat_host_begin_p2p(handle, c"nothing".as_ptr(), &raw mut ours) },
            LOWLAT_ERR_UNKNOWN_ATTEMPT
        );

        // A withdrawal that overtook its own offer.
        unsafe { lowlat_host_end_connection(handle, c"late".as_ptr()) };
        let late = attempt("late");
        assert_eq!(
            unsafe { lowlat_host_new_attempt(handle, &raw const late) },
            LOWLAT_ERR_WITHDRAWN
        );

        // Approving twice.
        let info = attempt("a");
        assert_eq!(
            unsafe { lowlat_host_new_attempt(handle, &raw const info) },
            LOWLAT_OK
        );
        assert_eq!(
            unsafe { lowlat_host_begin_p2p(handle, c"a".as_ptr(), &raw mut ours) },
            LOWLAT_OK
        );
        assert_eq!(
            unsafe { lowlat_host_begin_p2p(handle, c"a".as_ptr(), &raw mut ours) },
            LOWLAT_ERR_ALREADY_BEGUN
        );

        // And a full house. **Capacity counts guests that were approved, not
        // offers that were registered**, so filling it means approving each
        // one: registering costs bookkeeping and approving costs a socket and
        // a thread, and it is the second that the limit exists to bound.
        let mut ids: Vec<String> = Vec::new();
        for extra in 0..8 {
            let id = format!("guest{extra}");
            let info = attempt(&id);
            let status = unsafe { lowlat_host_new_attempt(handle, &raw const info) };
            if status == LOWLAT_ERR_AT_CAPACITY {
                for id in &ids {
                    let id = std::ffi::CString::new(id.as_str()).expect("no interior nul");
                    unsafe { lowlat_host_end_connection(handle, id.as_ptr()) };
                }
                unsafe { lowlat_host_end_connection(handle, c"a".as_ptr()) };
                unsafe { lowlat_destroy(handle) };
                return;
            }
            assert_eq!(status, LOWLAT_OK);
            let id_c = std::ffi::CString::new(id.as_str()).expect("no interior nul");
            assert_eq!(
                unsafe { lowlat_host_begin_p2p(handle, id_c.as_ptr(), &raw mut ours) },
                LOWLAT_OK
            );
            ids.push(id);
        }
        panic!("the guest limit was never reached");
    }

    /// **An attempt with nothing to address it by is refused**, because every
    /// later call in the seam finds it by that identifier.
    #[test]
    fn an_attempt_without_the_parts_that_identify_it_is_refused() {
        let handle = started();
        for spoil in [
            (|info: &mut lowlat_attempt_info| info.attempt_id = [0; LOWLAT_ATTEMPT_MAX])
                as fn(&mut lowlat_attempt_info),
            |info| info.ufrag = [0; LOWLAT_ICE_MAX],
            |info| info.pwd = [0; LOWLAT_ICE_MAX],
            // A field with no terminator was overrun by whoever filled it.
            |info| info.pwd = [b'x' as c_char; LOWLAT_ICE_MAX],
            |info| info.size = 4,
        ] {
            let mut info = attempt("a");
            spoil(&mut info);
            assert_eq!(
                unsafe { lowlat_host_new_attempt(handle, &raw const info) },
                LOWLAT_ERR_INVALID_ARGUMENT
            );
        }
        unsafe { lowlat_destroy(handle) };
    }

    /// **A readiness marker carries no address and is still forwarded**, while
    /// an ordinary candidate without one is refused. A peer may withhold every
    /// real candidate until it has seen the marker.
    #[test]
    fn a_readiness_marker_needs_no_address_and_a_candidate_does() {
        let handle = started();
        let info = attempt("a");
        assert_eq!(
            unsafe { lowlat_host_new_attempt(handle, &raw const info) },
            LOWLAT_OK
        );

        let mut cand = lowlat_candidate {
            size: u32::try_from(core::mem::size_of::<lowlat_candidate>()).unwrap_or(u32::MAX),
            port: 41000,
            sync: true,
            reserved: 0,
            address: [0; LOWLAT_ADDRESS_MAX],
        };
        // Accepted with nothing in the address at all.
        unsafe { lowlat_host_add_candidate(handle, c"a".as_ptr(), &raw const cand) };

        cand.sync = false;
        put(&mut cand.address, "192.168.1.100");
        unsafe { lowlat_host_add_candidate(handle, c"a".as_ptr(), &raw const cand) };

        // **An unknown attempt is a race with teardown, not a fault**, so it
        // is swallowed rather than reported.
        unsafe { lowlat_host_add_candidate(handle, c"gone".as_ptr(), &raw const cand) };

        unsafe { lowlat_host_end_connection(handle, c"a".as_ptr()) };
        unsafe { lowlat_destroy(handle) };
    }

    /// The seam needs a host. Registering against one that never started is a
    /// mistake worth naming rather than a silent no-op.
    #[test]
    fn the_seam_refuses_a_handle_that_is_not_hosting() {
        let handle = handle();
        let info = attempt("a");
        assert_eq!(
            unsafe { lowlat_host_new_attempt(handle, &raw const info) },
            LOWLAT_ERR_NOT_STARTED
        );
        unsafe { lowlat_destroy(handle) };
    }
}

#[cfg(test)]
mod roster_tests {
    use super::seam_tests::{approved, started};
    use super::*;

    /// Read the whole roster, which several tests need.
    pub(super) fn roster_of(handle: *mut lowlat) -> Vec<lowlat_guest> {
        let mut count = 0u32;
        assert_eq!(
            unsafe { lowlat_host_get_guests(handle, core::ptr::null_mut(), &raw mut count) },
            LOWLAT_OK
        );
        let mut room = vec![
            lowlat_guest {
                number: 0,
                permissions: lowlat_permissions {
                    keyboard: false,
                    pointer: false,
                    gamepad: false,
                    reserved: 0,
                },
                owner: false,
                reserved: [0; 3],
                attempt: [0; LOWLAT_ATTEMPT_MAX],
            };
            count as usize
        ];
        assert_eq!(
            unsafe { lowlat_host_get_guests(handle, room.as_mut_ptr(), &raw mut count) },
            LOWLAT_OK
        );
        room.truncate(count as usize);
        room
    }

    /// **Two calls, and the first one is allowed to answer with nothing.** An
    /// application asks how many before it decides what to allocate.
    #[test]
    fn the_count_comes_first_and_the_roster_second() {
        let handle = started();
        let mut count = 0u32;
        assert_eq!(
            unsafe { lowlat_host_get_guests(handle, core::ptr::null_mut(), &raw mut count) },
            LOWLAT_OK
        );
        assert_eq!(count, 0, "a host with no guests reported some");

        approved(handle, "a");
        approved(handle, "b");

        let mut count = 0u32;
        assert_eq!(
            unsafe { lowlat_host_get_guests(handle, core::ptr::null_mut(), &raw mut count) },
            LOWLAT_OK
        );
        assert_eq!(count, 2);

        let mut room = [lowlat_guest {
            number: 0,
            permissions: lowlat_permissions {
                keyboard: false,
                pointer: false,
                gamepad: false,
                reserved: 0,
            },
            owner: false,
            reserved: [0; 3],
            attempt: [0; LOWLAT_ATTEMPT_MAX],
        }; 4];
        let mut count = 4u32;
        assert_eq!(
            unsafe { lowlat_host_get_guests(handle, room.as_mut_ptr(), &raw mut count) },
            LOWLAT_OK
        );
        assert_eq!(count, 2);
        // **Ordered by number.** A roster that reshuffles itself is one an
        // application cannot diff against the last one it drew.
        assert!(room[0].number < room[1].number);
        assert!(room[0].permissions.keyboard && room[0].permissions.pointer);

        unsafe { lowlat_destroy(handle) };
    }

    /// **A buffer too small is filled as far as it goes and says what it
    /// needed.** The roster moves; a caller that sized its array a moment ago
    /// must not lose the call for it.
    #[test]
    fn a_roster_larger_than_the_buffer_reports_what_it_needed() {
        let handle = started();
        approved(handle, "a");
        approved(handle, "b");

        let mut room = [lowlat_guest {
            number: 0,
            permissions: lowlat_permissions {
                keyboard: false,
                pointer: false,
                gamepad: false,
                reserved: 0,
            },
            owner: false,
            reserved: [0; 3],
            attempt: [0; LOWLAT_ATTEMPT_MAX],
        }; 1];
        let mut count = 1u32;
        assert_eq!(
            unsafe { lowlat_host_get_guests(handle, room.as_mut_ptr(), &raw mut count) },
            LOWLAT_ERR_TOO_SMALL
        );
        assert_eq!(count, 2, "it did not say how many there really were");
        assert_ne!(room[0].number, 0, "the room it had was left unfilled");

        unsafe { lowlat_destroy(handle) };
    }

    /// **The roster reaches everybody and is not a message.** A peer has no way
    /// to ask for one, and it finds itself in the list by number; a host that
    /// never sends one leaves every guest not knowing what it is.
    #[test]
    fn a_roster_reaches_every_guest_and_an_empty_room_is_not_a_failure() {
        let handle = started();
        let body = br#"[{"id":1}]"#;

        // Nobody seated: reaching zero guests is what an empty room means.
        let mut reached = u32::MAX;
        assert_eq!(
            unsafe {
                lowlat_host_send_roster(
                    handle,
                    body.as_ptr().cast(),
                    u32::try_from(body.len()).unwrap_or(0),
                    &raw mut reached,
                )
            },
            LOWLAT_OK
        );
        assert_eq!(reached, 0);

        approved(handle, "a");
        approved(handle, "b");
        let mut reached = 0u32;
        assert_eq!(
            unsafe {
                lowlat_host_send_roster(
                    handle,
                    body.as_ptr().cast(),
                    u32::try_from(body.len()).unwrap_or(0),
                    &raw mut reached,
                )
            },
            LOWLAT_OK
        );
        assert_eq!(reached, 2, "the roster did not reach both guests");

        // Past what a peer accepts, refused here rather than at a far end that
        // says nothing about why it vanished.
        let huge = vec![b'x'; lowlat_core::control::USER_DATA_MAX];
        assert_eq!(
            unsafe {
                lowlat_host_send_roster(
                    handle,
                    huge.as_ptr().cast(),
                    u32::try_from(huge.len()).unwrap_or(u32::MAX),
                    core::ptr::null_mut(),
                )
            },
            LOWLAT_ERR_INVALID_ARGUMENT
        );

        unsafe { lowlat_destroy(handle) };
    }

    /// A message aimed at nobody in particular reaches everyone, and one aimed
    /// at a guest that is not there says so.
    #[test]
    fn a_message_reaches_a_guest_or_says_it_could_not() {
        let handle = started();
        approved(handle, "a");

        let body = b"hello";
        assert_eq!(
            unsafe {
                lowlat_host_send_user_data(
                    handle,
                    LOWLAT_GUEST_ALL,
                    9,
                    body.as_ptr().cast(),
                    u32::try_from(body.len()).unwrap_or(0),
                )
            },
            LOWLAT_OK
        );
        assert_eq!(
            unsafe { lowlat_host_send_user_data(handle, 4242, 9, body.as_ptr().cast(), 5) },
            LOWLAT_ERR_UNKNOWN_GUEST
        );

        // **A body past what a peer accepts is refused here**, where the
        // caller can still do something, rather than at a far end that says
        // nothing about why it vanished.
        let huge = vec![b'x'; lowlat_core::control::USER_DATA_MAX];
        assert_eq!(
            unsafe {
                lowlat_host_send_user_data(
                    handle,
                    LOWLAT_GUEST_ALL,
                    9,
                    huge.as_ptr().cast(),
                    u32::try_from(huge.len()).unwrap_or(u32::MAX),
                )
            },
            LOWLAT_ERR_INVALID_ARGUMENT
        );

        // An empty body is a message, not a mistake: the sub-identifier alone
        // is what some of them mean.
        assert_eq!(
            unsafe {
                lowlat_host_send_user_data(handle, LOWLAT_GUEST_ALL, 9, core::ptr::null(), 0)
            },
            LOWLAT_OK
        );

        unsafe { lowlat_destroy(handle) };
    }
}

#[cfg(test)]
mod guest_tests {
    use super::roster_tests::roster_of;
    use super::seam_tests::{approved, started};
    use super::*;

    /// **Permissions are recorded here and applied there**, so the roster
    /// answers with the new ones straight away while the guest's own devices
    /// pick them up on its next pass.
    #[test]
    fn changed_permissions_show_in_the_roster_at_once() {
        let handle = started();
        approved(handle, "a");
        let before = roster_of(handle);
        assert!(before[0].permissions.keyboard && before[0].permissions.gamepad);

        let perms = lowlat_permissions {
            keyboard: false,
            pointer: true,
            gamepad: false,
            reserved: 0,
        };
        assert_eq!(
            unsafe { lowlat_host_set_permissions(handle, before[0].number, &raw const perms) },
            LOWLAT_OK
        );
        let after = roster_of(handle);
        assert!(
            !after[0].permissions.keyboard && after[0].permissions.pointer,
            "the roster still reports what signaling said, not what was set"
        );

        assert_eq!(
            unsafe { lowlat_host_set_permissions(handle, 4242, &raw const perms) },
            LOWLAT_ERR_UNKNOWN_GUEST
        );
        unsafe { lowlat_destroy(handle) };
    }

    /// **A guest is kicked with a reason, and zero is not one.** A peer carries
    /// on through a status of zero, so kicking with it tells the guest nothing
    /// and leaves it exactly where it was.
    #[test]
    fn a_kick_needs_a_reason_a_peer_will_stop_on() {
        let handle = started();
        approved(handle, "a");
        let guest = roster_of(handle)[0].number;

        assert_eq!(
            unsafe { lowlat_host_kick_guest(handle, guest, 0) },
            LOWLAT_ERR_INVALID_ARGUMENT,
            "a status a peer ignores was accepted as a reason to end it"
        );
        assert_eq!(
            unsafe { lowlat_host_kick_guest(handle, 4242, -15000) },
            LOWLAT_ERR_UNKNOWN_GUEST
        );
        assert_eq!(
            unsafe { lowlat_host_kick_guest(handle, guest, -15000) },
            LOWLAT_OK
        );
        unsafe { lowlat_destroy(handle) };
    }
}

#[cfg(test)]
mod preflight_tests {
    use super::*;
    use crate::display::Capturable;

    /// **Each way of not being able to capture keeps its own status.** The two
    /// are indistinguishable once hosting has failed, which is the whole
    /// reason for asking beforehand: measured on a real display, an
    /// unprivileged process in the `video` group enumerates the output and
    /// reads its framebuffer and still gets no buffer handles, while the same
    /// binary as root gets them.
    #[test]
    fn every_answer_the_preflight_can_give_has_its_own_status() {
        for (found, expected) in [
            (Capturable::Yes, LOWLAT_OK),
            (Capturable::NothingLit, LOWLAT_ERR_NO_DISPLAY),
            (Capturable::NotReachable, LOWLAT_ERR_DISPLAY_UNREACHABLE),
        ] {
            assert_eq!(status_of(found), expected);
        }
        // And the live answer is one of them rather than something else.
        let answer = lowlat_can_host();
        assert!(
            [
                LOWLAT_OK,
                LOWLAT_ERR_NO_DISPLAY,
                LOWLAT_ERR_DISPLAY_UNREACHABLE
            ]
            .contains(&answer),
            "the pre-flight answered {answer:?}"
        );
    }
}

#[cfg(test)]
mod status_tests {
    use super::seam_tests::approved;
    use super::start_tests::{config, handle};
    use super::*;

    fn empty_status() -> lowlat_host_status {
        lowlat_host_status {
            size: u32::try_from(core::mem::size_of::<lowlat_host_status>()).unwrap_or(u32::MAX),
            guests: 0,
            width: 0,
            height: 0,
            running: false,
            audio_active: false,
            reserved: [0; 2],
            audio_device: [0; LOWLAT_OUTPUT_MAX],
        }
    }

    /// **A handle that is not hosting answers too.** An application asking
    /// what state something is in should not have to know the answer to ask
    /// the question.
    #[test]
    fn status_answers_before_hosting_and_after() {
        let handle = handle();
        let mut status = empty_status();
        assert_eq!(
            unsafe { lowlat_host_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert!(!status.running && status.guests == 0);

        let cfg = config();
        assert_eq!(
            unsafe { lowlat_host_start(handle, &raw const cfg) },
            LOWLAT_OK
        );
        approved(handle, "a");
        let mut status = empty_status();
        assert_eq!(
            unsafe { lowlat_host_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert!(status.running);
        assert_eq!(status.guests, 1);

        // **Zero before a display has been opened**, which is the honest
        // answer rather than the size that was configured -- there is no such
        // size, and reporting one would describe a stream nobody is making.
        assert_eq!((status.width, status.height), (0, 0));

        // And sound the same way: nothing is being read here, whatever it is
        // configured to do, so the device it is on is nothing rather than the
        // name that was asked for.
        assert!(!status.audio_active);
        assert_eq!(taken(&status.audio_device), Some(""));

        unsafe { lowlat_destroy(handle) };
    }
}

#[cfg(test)]
mod logging_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// **Only this test's own lines are counted.** The sink is process-wide
    /// and every other test in this binary logs into it, so counting
    /// everything would make this pass or fail on what else happened to be
    /// running.
    const MARK: &str = "logtest:";

    static SEEN: AtomicU32 = AtomicU32::new(0);
    static LEVEL_SEEN: AtomicU32 = AtomicU32::new(99);
    static TERMINATED: AtomicU32 = AtomicU32::new(0);
    static OPAQUE_KEPT: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn counted(level: u32, message: *const c_char, opaque: *mut c_void) {
        if message.is_null() {
            return;
        }
        // **Read as a C string, which is the whole reason for the copy.** A
        // message that was not terminated would run off the end here rather
        // than parse, so reaching this at all is half the assertion.
        let text = unsafe { CStr::from_ptr(message) };
        let Ok(text) = text.to_str() else {
            return;
        };
        if !text.starts_with(MARK) {
            return;
        }
        TERMINATED.fetch_add(1, Ordering::Relaxed);
        if opaque as usize == 0x1234 {
            OPAQUE_KEPT.fetch_add(1, Ordering::Relaxed);
        }
        LEVEL_SEEN.store(level, Ordering::Relaxed);
        SEEN.fetch_add(1, Ordering::Relaxed);
    }

    /// Forward to standard error, so clearing the callback at the end of this
    /// test does not silence every test that runs after it.
    unsafe extern "C" fn to_stderr(level: u32, message: *const c_char, _opaque: *mut c_void) {
        if message.is_null() {
            return;
        }
        let text = unsafe { CStr::from_ptr(message) };
        eprintln!("[{level}] {}", text.to_string_lossy());
    }

    /// **The line reaches the application terminated, with its own pointer
    /// handed back**, and the level still decides what is formatted at all.
    #[test]
    fn a_registered_callback_receives_lines_and_its_own_pointer() {
        assert_eq!(
            unsafe { lowlat_set_log_callback(Some(counted), 0x1234 as *mut c_void) },
            LOWLAT_OK
        );
        SEEN.store(0, Ordering::Relaxed);
        lowlat_common::log_warn!("{MARK} a line, key=value");
        assert_eq!(SEEN.load(Ordering::Relaxed), 1);
        assert_eq!(TERMINATED.load(Ordering::Relaxed), 1);
        assert_eq!(
            OPAQUE_KEPT.load(Ordering::Relaxed),
            1,
            "the opaque pointer did not survive the round trip"
        );
        assert_eq!(
            LEVEL_SEEN.load(Ordering::Relaxed),
            lowlat_log_level::LOWLAT_LOG_WARN as u32
        );

        // **Replaceable**, which the sink underneath is not: clearing stops
        // delivery rather than being refused because something is installed.
        assert_eq!(
            unsafe { lowlat_set_log_callback(None, core::ptr::null_mut()) },
            LOWLAT_OK
        );
        lowlat_common::log_warn!("{MARK} after clearing");
        assert_eq!(
            SEEN.load(Ordering::Relaxed),
            1,
            "a line arrived after the callback was cleared"
        );

        // A level nothing defines is refused rather than quietly clamped.
        assert_eq!(lowlat_set_log_level(99), LOWLAT_ERR_INVALID_ARGUMENT);
        assert_eq!(
            lowlat_set_log_level(lowlat_log_level::LOWLAT_LOG_INFO as u32),
            LOWLAT_OK
        );

        unsafe { lowlat_set_log_callback(Some(to_stderr), core::ptr::null_mut()) };
    }
}

#[cfg(test)]
mod metrics_tests {
    use super::roster_tests::roster_of;
    use super::seam_tests::{approved, started};
    use super::*;

    fn empty() -> lowlat_metrics {
        lowlat_metrics {
            size: u32::try_from(core::mem::size_of::<lowlat_metrics>()).unwrap_or(u32::MAX),
            connected_ms: 0,
            keyboard_ms: 0,
            pointer_ms: 0,
            gamepad_ms: 0,
            frames: 0,
            window: 0,
            stale: 0,
            cg_events: 0,
            bitrate_mbps: 0.0,
            encode_ms: 0.0,
            network_ms: 0.0,
        }
    }

    /// **A guest carries the attempt it was registered under.** Everything
    /// before a guest is seated is addressed by attempt and everything after by
    /// number; without the link an application holding one peer per attempt
    /// cannot tell which peer an event about a guest concerns.
    #[test]
    fn a_guest_carries_the_attempt_it_was_registered_under() {
        let handle = started();
        approved(handle, "an-attempt-with-a-name");

        let roster = roster_of(handle);
        assert_eq!(roster.len(), 1);
        assert_eq!(
            taken(&roster[0].attempt).unwrap_or_default(),
            "an-attempt-with-a-name"
        );
        unsafe { lowlat_destroy(handle) };
    }

    /// **Never is not zero milliseconds ago.** An application kicking idle
    /// guests reads these two the same way and must be able to tell them apart:
    /// a guest that has touched nothing since it arrived is not a guest that
    /// touched the keyboard as the session opened.
    #[test]
    fn metrics_answer_for_a_seated_guest_and_say_never_where_nothing_happened() {
        let handle = started();
        approved(handle, "a");
        let guest = roster_of(handle)[0].number;

        let mut metrics = empty();
        assert_eq!(
            unsafe { lowlat_host_get_metrics(handle, guest, &raw mut metrics) },
            LOWLAT_OK
        );
        assert_eq!(
            metrics.keyboard_ms, 0,
            "a guest that has sent nothing reported a time it last did"
        );
        assert_eq!(metrics.pointer_ms, 0);
        assert_eq!(metrics.gamepad_ms, 0);

        assert_eq!(
            unsafe { lowlat_host_get_metrics(handle, 4242, &raw mut metrics) },
            LOWLAT_ERR_UNKNOWN_GUEST
        );
        // The size field is the versioning, and one saying less than the
        // structure holds means the caller and the header disagree.
        let mut stale = empty();
        stale.size = 4;
        assert_eq!(
            unsafe { lowlat_host_get_metrics(handle, guest, &raw mut stale) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );

        unsafe { lowlat_destroy(handle) };
    }
}
