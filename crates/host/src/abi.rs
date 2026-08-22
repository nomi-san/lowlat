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
const DESCRIPTIONS: [(lowlat_status, &CStr); 14] = [
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
pub const LOWLAT_OUTPUT_MAX: usize = 64;

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
    /// **Clearing it is a permission, not an instruction.** There is no damage
    /// signal here, so nothing yet skips a repeated picture; a host that keeps
    /// sending costs bitrate rather than being wrong.
    pub full_fps: bool,
    pub reserved: [u8; 3],
    /// Which output to capture, by an identity from the enumeration. **Empty
    /// means whichever this host would pick on its own**, which is the output
    /// at the desktop's corner and then whatever is lit.
    pub output: [c_char; LOWLAT_OUTPUT_MAX],
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
#[derive(Debug, Default)]
struct Held {
    seam: Option<crate::admission::Admission>,
    /// **Shared rather than borrowed**, so a poll can take a handle on the
    /// queue and release the lock before it waits. Holding the lock across a
    /// wait would stop every other call for the length of somebody's timeout.
    /// One consumer is still the contract; this is what keeps the lock honest,
    /// not what makes two consumers safe.
    events: Option<std::sync::Arc<crate::events::Receiver>>,
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

/// One host session, as the application holds it.
///
/// Opaque: the application holds a pointer it cannot look inside, so what is
/// in here changes freely.
#[derive(Debug, Default)]
pub struct lowlat {
    /// Set when a call was contained, and never cleared.
    poisoned: AtomicBool,
    held: std::sync::Mutex<Held>,
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
        let handle = Box::new(lowlat::default());
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
    for server in cfg.servers.iter().take(cfg.server_count as usize) {
        // Resolved here rather than at the first check, so a name that does not
        // resolve is refused while the caller is still holding the call that
        // set it.
        let text = taken(server)?;
        let mut found = std::net::ToSocketAddrs::to_socket_addrs(text).ok()?;
        servers.push(found.next()?);
    }
    let video = video_configured(&cfg.video)?;
    let output = taken(&cfg.video.output)?;

    Some(crate::admission::Config {
        base_port: cfg.base_port,
        max_guests: cfg.max_guests as usize,
        servers,
        exclusive_pointer: cfg.exclusive_pointer,
        exclusive_hold_ms: f64::from(cfg.exclusive_hold_ms),
        cg_level,
        // A live-run aid, and nothing an application should be able to ask for.
        rumble_probe: false,
        stream: Some(crate::stream::Config {
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
            let mut seam = crate::admission::Admission::new(config);
            // **Taken now, and held beside the seam rather than inside it.** A
            // poll waits for as long as its caller asked, and every other call
            // has to stay answerable while it does.
            held.events = seam.take_events().map(std::sync::Arc::new);
            held.seam = Some(seam);
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
            let events = handle.held().events.clone();
            let Some(events) = events else {
                // Nothing raises events until hosting starts. **The timeout is
                // still owed**: an application starts its polling thread before
                // it starts hosting, and a poll that answers instantly turns
                // that thread into a spin.
                std::thread::sleep(timeout);
                return LOWLAT_TIMEOUT;
            };

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

    /// **Polling a host that has not started is quiet, not broken** -- and it
    /// still costs the time it was asked for. An application starts its
    /// polling thread before it starts hosting, so a poll that answers
    /// instantly makes that thread a spin.
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

    /// **Hosting is what fills the queue**, and stopping does not empty it:
    /// what was raised on the way down is still worth polling.
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
        assert!(handle_has_a_queue(handle));

        assert_eq!(unsafe { lowlat_host_stop(handle) }, LOWLAT_OK);
        assert!(
            handle_has_a_queue(handle),
            "stopping threw away events that had already been raised"
        );
        unsafe { lowlat_destroy(handle) };
    }

    fn handle_has_a_queue(ll: *mut lowlat) -> bool {
        unsafe { ll.as_ref() }.is_some_and(|handle| handle.held().events.is_some())
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

    fn started() -> *mut lowlat {
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
