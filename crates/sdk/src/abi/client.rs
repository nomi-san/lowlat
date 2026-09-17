//! The client half of the public C ABI.
//!
//! Every `lowlat_client_*` entry point and every type it takes or fills.
//! Behind the `client` feature, which is what lets a build carry a host and
//! no client. The seam is the host's, mirrored ([06 §3b](../docs/06-api.md)):
//! a client makes the offer, so the attempt produces the credentials the
//! application puts in it, candidates come out as events, and beginning takes
//! what the answer carried.

use core::ffi::{c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use ::lowlat_client::{Client, Event, Outcome};
use lowlat_common::events::Delivery;
use lowlat_event_type::*;
use lowlat_outcome::*;

use super::guard;
use super::lowlat_status::{self, *};
use super::shared::*;

/// What a client is created with.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_create_info {
    /// Set by the caller to `sizeof(lowlat_client_create_info)`.
    pub size: u32,
}

/// What a client asks of a host, per attempt.
///
/// **Zeroed is the sensible default**: no size request, compressed sound, the
/// current cipher, no reflexive servers.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_config {
    /// Set by the caller to `sizeof(lowlat_client_config)`.
    pub size: u32,
    /// The picture size asked of the host, or zero for no preference.
    ///
    /// **A request to change the host's display, not a description of this
    /// one.** An established host takes the owner's figure as a mode request,
    /// so set it only to change the person's monitor.
    pub resolution_x: u32,
    pub resolution_y: u32,
    /// Whether uncompressed sound is acceptable.
    pub raw_audio: bool,
    /// Offer no media key, so the host answers without one and both ends key
    /// the legacy 128-bit cipher from its certificate digest.
    pub legacy_cipher: bool,
    /// Offer addresses from the carrier-grade shared range as candidates.
    pub shared_address_space: bool,
    pub reserved: u8,
    /// How many of `servers` are set.
    pub server_count: u32,
    /// Reflexive servers, consulted for this client's own mapped address,
    /// each as `host:port`, NUL-terminated.
    pub servers: [[c_char; LOWLAT_SERVER_MAX]; LOWLAT_SERVERS_MAX],
}

/// The session as it stands.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_status {
    /// Set by the caller to `sizeof(lowlat_client_status)`.
    pub size: u32,
    /// [`LOWLAT_CLIENT_CONNECTING`], [`LOWLAT_CLIENT_ESTABLISHED`] or
    /// [`LOWLAT_CLIENT_OVER`]; [`LOWLAT_CLIENT_IDLE`] with no attempt.
    pub state: u32,
    /// The status the host's disconnect carried, or zero.
    pub disconnect: i32,
    /// The smoothed round trip to the host, in milliseconds.
    pub rtt_ms: u32,
    /// Messages arrived on the video channel and not yet consumed: what the
    /// reader is behind by.
    pub behind: u32,
    /// How long there has been anything unconsumed, in milliseconds.
    pub behind_ms: u32,
    /// Pictures taken off the video channel.
    pub pictures: u64,
    /// Pictures the catch-up discarded to land on a keyframe.
    pub skipped: u64,
    /// Sound packets taken off the audio channel.
    pub audio_packets: u64,
}

/// No attempt has been made.
pub const LOWLAT_CLIENT_IDLE: u32 = 0;
/// Connectivity is running.
pub const LOWLAT_CLIENT_CONNECTING: u32 = 1;
/// The session is up.
pub const LOWLAT_CLIENT_ESTABLISHED: u32 = 2;
/// The session is over; the ended event says why.
pub const LOWLAT_CLIENT_OVER: u32 = 3;

/// One client, as the application holds it.
///
/// Opaque: the application holds a pointer it cannot look inside, so what is
/// in here changes freely.
#[derive(Debug)]
pub struct lowlat_client {
    /// Set when a call was contained, and never cleared.
    poisoned: AtomicBool,
    held: std::sync::Mutex<Held>,
    /// Held outside the lock because a poll waits for as long as its caller
    /// asked and every other call must stay answerable while it does.
    events: lowlat_common::events::Receiver<Event>,
}

#[derive(Debug)]
struct Held {
    seam: Client,
    /// The one attempt's identifier, so an ending addressed to the client
    /// alone can name it to the seam.
    attempt: Option<String>,
}

impl lowlat_client {
    /// **A poisoned lock is not a second failure to report.** The handle is
    /// already refusing every call once a panic has been contained.
    fn held(&self) -> std::sync::MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(|held| held.into_inner())
    }
}

/// Create a handle.
///
/// @param[in] info One [`lowlat_client_create_info`] whose `size` says how much of it is set.
/// May be null, which takes every default.
/// @param[out] out Receives the handle.
/// @returns [`LOWLAT_OK`], or an error and `out` left untouched.
///
/// # Safety
///
/// `out` must point to storage for one pointer. `info` may be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_create(
    info: *const lowlat_client_create_info,
    out: *mut *mut lowlat_client,
) -> lowlat_status {
    guard(LOWLAT_ERR_INTERNAL, || {
        if out.is_null() {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        if let Some(info) = (unsafe { info.as_ref() })
            && (info.size as usize) < core::mem::size_of::<u32>()
        {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let mut seam = Client::new();
        let Some(events) = seam.take_events() else {
            return LOWLAT_ERR_INTERNAL;
        };
        let handle = Box::new(lowlat_client {
            poisoned: AtomicBool::new(false),
            held: std::sync::Mutex::new(Held {
                seam,
                attempt: None,
            }),
            events,
        });
        unsafe { out.write(Box::into_raw(handle)) };
        LOWLAT_OK
    })
}

/// Destroy a handle, leaving any session it holds.
///
/// **Works on a poisoned handle**, which is the point of poisoning.
///
/// @param[in] cl The handle from [`lowlat_client_create`], not used again. Null is accepted
/// and does nothing.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`] and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_destroy(cl: *mut lowlat_client) {
    guard((), || {
        if cl.is_null() {
            return;
        }
        drop(unsafe { Box::from_raw(cl) });
    });
}

/// Turn what the application wrote into what the seam takes.
fn configured(cfg: &lowlat_client_config) -> Option<::lowlat_client::Config> {
    let mut servers = Vec::new();
    for server in cfg.servers.iter().take(cfg.server_count as usize) {
        let text = taken(server)?;
        if text.is_empty() {
            continue;
        }
        let found = ::lowlat_net::addrs::resolve_server(text);
        if found.is_empty() {
            return None;
        }
        for addr in found {
            if servers.len() < LOWLAT_SERVERS_MAX {
                servers.push(addr);
            }
        }
    }
    Some(::lowlat_client::Config {
        resolution: (cfg.resolution_x, cfg.resolution_y),
        raw_audio: cfg.raw_audio,
        legacy_cipher: cfg.legacy_cipher,
        servers,
        shared_address_space: cfg.shared_address_space,
        ..::lowlat_client::Config::default()
    })
}

fn refused(error: ::lowlat_client::Error) -> lowlat_status {
    use ::lowlat_client::Error;
    match error {
        Error::Busy => LOWLAT_ERR_ALREADY_STARTED,
        Error::UnknownAttempt => LOWLAT_ERR_UNKNOWN_ATTEMPT,
        Error::AlreadyBegun => LOWLAT_ERR_ALREADY_BEGUN,
        Error::Transport | Error::Credentials => LOWLAT_ERR_INVALID_ARGUMENT,
        Error::Crypto => LOWLAT_ERR_CRYPTO,
        Error::Io => LOWLAT_ERR_IO,
    }
}

/// Begin an attempt: mint the credentials the offer carries.
///
/// **Nothing is sent and no socket is opened.** The application puts what
/// comes back into its offer over its own signaling and calls
/// [`lowlat_client_begin_p2p`] with the answer. One attempt at a time; a second
/// while one exists is refused with [`LOWLAT_ERR_ALREADY_STARTED`].
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] cfg What to ask of the host. May be null, which takes every default.
/// @param[in] attempt_id The identifier every later call and event names, NUL-terminated.
/// @param[in] transport A [`lowlat_transport`] value. Only the native one is accepted.
/// @param[out] ours Filled with the credentials for the offer. `port` is zero: the socket
/// does not exist yet.
/// @returns [`LOWLAT_OK`], [`LOWLAT_ERR_ALREADY_STARTED`], [`LOWLAT_ERR_INVALID_ARGUMENT`]
/// for the browser pipe or a server that does not resolve, or [`LOWLAT_ERR_CRYPTO`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `attempt_id` is NUL-terminated;
/// `cfg` is null or points to one [`lowlat_client_config`] whose `size` says
/// how much of it is set; `ours` points to one [`lowlat_credentials`] whose
/// `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_new_attempt(
    cl: *mut lowlat_client,
    cfg: *const lowlat_client_config,
    attempt_id: *const c_char,
    transport: u32,
    ours: *mut lowlat_credentials,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let (Some(attempt), Some(slot)) = (read_c_str(attempt_id), ours.as_mut()) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if attempt.is_empty()
                || (slot.size as usize) < core::mem::size_of::<lowlat_credentials>()
            {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let transport = match transport {
                x if x == lowlat_transport::LOWLAT_TRANSPORT_BUD as u32 => {
                    ::lowlat_client::Transport::Bud
                }
                x if x == lowlat_transport::LOWLAT_TRANSPORT_WEB as u32 => {
                    ::lowlat_client::Transport::Web
                }
                _ => return LOWLAT_ERR_INVALID_ARGUMENT,
            };
            let config = match cfg.as_ref() {
                Some(cfg) => {
                    if (cfg.size as usize) < core::mem::size_of::<lowlat_client_config>() {
                        return LOWLAT_ERR_INVALID_ARGUMENT;
                    }
                    match configured(cfg) {
                        Some(config) => config,
                        None => return LOWLAT_ERR_INVALID_ARGUMENT,
                    }
                }
                None => ::lowlat_client::Config::default(),
            };
            let mut held = handle.held();
            let credentials = match held.seam.new_attempt(attempt, config, transport) {
                Ok(credentials) => credentials,
                Err(error) => return refused(error),
            };
            held.attempt = Some(attempt.to_string());
            slot.port = 0;
            slot.reserved = 0;
            put(&mut slot.ufrag, &credentials.ufrag);
            put(&mut slot.pwd, &credentials.pwd);
            put(&mut slot.fingerprint, &credentials.fingerprint);
            put(&mut slot.aes256, &credentials.aes256);
            LOWLAT_OK
        })
    }
}

/// Offer one address the host might be reachable at.
///
/// **An unknown attempt is accepted silently**: a candidate can arrive after
/// the attempt was ended, and that is a race rather than a fault.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] attempt_id The attempt this address belongs to, NUL-terminated.
/// @param[in] cand The candidate, `size` set.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `attempt_id` is NUL-terminated;
/// `cand` points to one [`lowlat_candidate`] whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_add_candidate(
    cl: *mut lowlat_client,
    attempt_id: *const c_char,
    cand: *const lowlat_candidate,
) {
    unsafe {
        entered(cl, |handle| {
            let (Some(attempt), Some(cand)) = (read_c_str(attempt_id), cand.as_ref()) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (cand.size as usize) < core::mem::size_of::<lowlat_candidate>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let Some(address) = taken(&cand.address) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            let addr = match address.parse::<std::net::IpAddr>() {
                Ok(ip) => std::net::SocketAddr::new(ip, cand.port),
                Err(_) if cand.sync => std::net::SocketAddr::from(([0, 0, 0, 0], cand.port)),
                Err(_) => return LOWLAT_ERR_INVALID_ARGUMENT,
            };
            let kind = lowlat_core::conn::Kind::marked(cand.lan, cand.reflexive);
            handle
                .held()
                .seam
                .add_candidate(attempt, addr, cand.sync, kind);
            LOWLAT_OK
        });
    }
}

/// The answer arrived: bind, start connectivity, and begin trickling
/// candidates as events.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] attempt_id The attempt the answer is for, NUL-terminated.
/// @param[in] theirs The host's credentials from the answer, `size` set. An empty `aes256`
/// selects the legacy cipher, keyed from `fingerprint`.
/// @returns [`LOWLAT_OK`], [`LOWLAT_ERR_UNKNOWN_ATTEMPT`], [`LOWLAT_ERR_ALREADY_BEGUN`],
/// [`LOWLAT_ERR_INVALID_ARGUMENT`] for credentials that cannot key a session,
/// [`LOWLAT_ERR_IO`] or [`LOWLAT_ERR_CRYPTO`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `attempt_id` is NUL-terminated;
/// `theirs` points to one [`lowlat_credentials`] whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_begin_p2p(
    cl: *mut lowlat_client,
    attempt_id: *const c_char,
    theirs: *const lowlat_credentials,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let (Some(attempt), Some(theirs)) = (read_c_str(attempt_id), theirs.as_ref()) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (theirs.size as usize) < core::mem::size_of::<lowlat_credentials>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let (Some(ufrag), Some(pwd), Some(fingerprint), Some(aes256)) = (
                taken(&theirs.ufrag),
                taken(&theirs.pwd),
                taken(&theirs.fingerprint),
                taken(&theirs.aes256),
            ) else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if ufrag.is_empty() || pwd.is_empty() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let peer = ::lowlat_client::Peer {
                ufrag: ufrag.to_string(),
                pwd: pwd.to_string(),
                fingerprint: fingerprint.to_string(),
                aes256: (!aes256.is_empty()).then(|| aes256.to_string()),
            };
            match handle.held().seam.begin_p2p(attempt, &peer) {
                Ok(()) => LOWLAT_OK,
                Err(error) => refused(error),
            }
        })
    }
}

/// Leave the session, or abandon an attempt that never began.
///
/// A session that is up is told, on the control channel, that this client is
/// leaving cleanly; the message is given a moment to arrive. **No event is
/// raised**: the application caused this. A handle with no attempt is left as
/// it is.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_end_connection(cl: *mut lowlat_client) {
    unsafe {
        entered(cl, |handle| {
            let mut held = handle.held();
            if let Some(attempt) = held.attempt.take() {
                held.seam.end_connection(&attempt);
            }
            LOWLAT_OK
        });
    }
}

/// Send the host's application a message.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] id The sub-identifier, which means whatever the two applications agreed.
/// @param[in] data The body. A terminator is added; one already there is not doubled.
/// @param[in] len How many bytes of `data`.
/// @returns [`LOWLAT_OK`], [`LOWLAT_ERR_NOT_STARTED`] with no session up, or
/// [`LOWLAT_ERR_INVALID_ARGUMENT`] past the message ceiling.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `data` points to `len` readable
/// bytes, or is null with `len` zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_user_data(
    cl: *mut lowlat_client,
    id: u32,
    data: *const c_void,
    len: u32,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let text: &[u8] = if data.is_null() {
                if len != 0 {
                    return LOWLAT_ERR_INVALID_ARGUMENT;
                }
                &[]
            } else {
                core::slice::from_raw_parts(data.cast::<u8>(), len as usize)
            };
            let text = text.strip_suffix(&[0]).unwrap_or(text);
            if lowlat_core::control::string_body_len(text.len())
                > lowlat_core::control::USER_DATA_MAX
            {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            if handle.held().seam.send_user_data(id, text) {
                LOWLAT_OK
            } else {
                LOWLAT_ERR_NOT_STARTED
            }
        })
    }
}

/// Where the session stands.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[out] out One [`lowlat_client_status`] with `size` set, filled.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `out` points to one
/// [`lowlat_client_status`] whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_get_status(
    cl: *mut lowlat_client,
    out: *mut lowlat_client_status,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let Some(out) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (out.size as usize) < core::mem::size_of::<lowlat_client_status>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let t = held.seam.telemetry();
            #[allow(
                clippy::cast_possible_wrap,
                reason = "a status is signed and travels in an unsigned word"
            )]
            let disconnect = t.disconnect.load(Ordering::Relaxed) as i32;
            let state = if held.attempt.is_none() {
                LOWLAT_CLIENT_IDLE
            } else {
                match t.state.load(Ordering::Relaxed) {
                    1 => LOWLAT_CLIENT_ESTABLISHED,
                    2 => LOWLAT_CLIENT_OVER,
                    _ => LOWLAT_CLIENT_CONNECTING,
                }
            };
            *out = lowlat_client_status {
                size: out.size,
                state,
                disconnect,
                rtt_ms: t.rtt_ms.load(Ordering::Relaxed),
                behind: t.behind.load(Ordering::Relaxed),
                behind_ms: t.behind_ms.load(Ordering::Relaxed),
                pictures: t.pictures.load(Ordering::Relaxed),
                skipped: t.skipped.load(Ordering::Relaxed),
                audio_packets: t.audio_packets.load(Ordering::Relaxed),
            };
            LOWLAT_OK
        })
    }
}

/// Take the next event, waiting up to `timeout_ms` for one.
///
/// The shape is [`lowlat_host_poll_events`]'s: a user-data body is copied into
/// `body` when one is offered, and one that does not fit is left where it was
/// with the length it needed written back.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] timeout_ms How long to wait. Zero polls.
/// @param[out] out The event.
/// @param[out] body Room for a body, or null to drop it.
/// @param[in,out] body_len How much room; on return, how much was written or needed.
/// @returns [`LOWLAT_OK`], [`LOWLAT_TIMEOUT`], [`LOWLAT_ERR_TOO_SMALL`], or
/// [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `out` points to one
/// [`lowlat_event`]; `body` is null or points to `*body_len` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_poll_events(
    cl: *mut lowlat_client,
    timeout_ms: u32,
    out: *mut lowlat_event,
    body: *mut c_void,
    body_len: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            if out.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            if !body.is_null() && body_len.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let timeout = Duration::from_millis(u64::from(timeout_ms));
            let attempt = handle.held().attempt.clone().unwrap_or_default();
            let events = &handle.events;

            if body.is_null() {
                let Some(received) = events.recv_timeout(timeout) else {
                    return LOWLAT_TIMEOUT;
                };
                out.write(described(&attempt, &received));
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
                    let event = described(&attempt, &received);
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

/// Describe one event in the shape the boundary publishes.
fn described(attempt: &str, received: &lowlat_common::events::Received<Event>) -> lowlat_event {
    let dropped = received.dropped;
    let mut named = [0; LOWLAT_ATTEMPT_MAX];
    put(&mut named, attempt);
    match &received.event {
        Event::Candidate {
            addr,
            from_stun,
            lan,
        } => {
            let mut body = lowlat_candidate_event {
                attempt: named,
                address: [0; LOWLAT_ADDRESS_MAX],
                port: 0,
                from_stun: *from_stun,
                lan: *lan,
            };
            put_address(&mut body.address, &mut body.port, addr);
            lowlat_event {
                kind: LOWLAT_EVENT_CANDIDATE,
                dropped,
                body: lowlat_event_body { candidate: body },
            }
        }
        Event::Ready => lowlat_event {
            kind: LOWLAT_EVENT_READY,
            dropped,
            body: lowlat_event_body {
                ready: lowlat_ready_event { attempt: named },
            },
        },
        Event::Established { addr } => {
            let mut body = lowlat_established_event {
                attempt: named,
                address: [0; LOWLAT_ADDRESS_MAX],
                port: 0,
                reserved: [0; 2],
            };
            put_address(&mut body.address, &mut body.port, addr);
            lowlat_event {
                kind: LOWLAT_EVENT_ESTABLISHED,
                dropped,
                body: lowlat_event_body { established: body },
            }
        }
        Event::Ended { outcome } => {
            // Exhaustive on purpose: a new way for an attempt to finish should
            // break this build rather than reach an application as a number it
            // has no name for.
            let (outcome, reason) = match outcome {
                Outcome::ConnectivityFailed => (LOWLAT_OUTCOME_CONNECTIVITY_FAILED, 0),
                Outcome::PeerGone => (LOWLAT_OUTCOME_PEER_GONE, 0),
                Outcome::Undeliverable => (LOWLAT_OUTCOME_UNDELIVERABLE, 0),
                Outcome::TransportFailed => (LOWLAT_OUTCOME_TRANSPORT_FAILED, 0),
                Outcome::Disconnected(status) => (LOWLAT_OUTCOME_DISCONNECTED, *status),
                Outcome::Unreadable => (LOWLAT_OUTCOME_CONTROL_STALLED, 0),
            };
            lowlat_event {
                kind: LOWLAT_EVENT_ENDED,
                dropped,
                body: lowlat_event_body {
                    ended: lowlat_ended_event {
                        attempt: named,
                        outcome,
                        reason,
                    },
                },
            }
        }
        Event::Blocked { blocked } => lowlat_event {
            kind: LOWLAT_EVENT_BLOCKED,
            dropped,
            body: lowlat_event_body {
                blocked: lowlat_blocked_event { blocked: *blocked },
            },
        },
        Event::StreamEnded { stream, status } => lowlat_event {
            kind: LOWLAT_EVENT_STREAM_ENDED,
            dropped,
            body: lowlat_event_body {
                stream_ended: lowlat_stream_ended_event {
                    stream: *stream,
                    status: *status,
                },
            },
        },
        Event::HostMode { mode } => lowlat_event {
            kind: LOWLAT_EVENT_HOST_MODE,
            dropped,
            body: lowlat_event_body {
                host_mode: lowlat_host_mode_event { mode: *mode },
            },
        },
        Event::UserData { id, text } => lowlat_event {
            kind: LOWLAT_EVENT_USER_DATA,
            dropped,
            body: lowlat_event_body {
                user_data: lowlat_user_data_event {
                    guest: 0,
                    id: *id,
                    body_len: u32::try_from(text.len()).unwrap_or(u32::MAX),
                },
            },
        },
    }
}

/// Run an entry point that needs the handle.
///
/// Null is refused, a poisoned handle is refused, and a panic poisons it.
///
/// # Safety
///
/// `cl` is null or came from [`lowlat_client_create`].
unsafe fn entered(
    cl: *mut lowlat_client,
    call: impl FnOnce(&lowlat_client) -> lowlat_status,
) -> lowlat_status {
    let Some(handle) = (unsafe { cl.as_ref() }) else {
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

#[cfg(test)]
#[allow(clippy::cast_possible_truncation)]
mod tests {
    use super::*;

    /// The whole seam through the boundary against nothing: an attempt is
    /// minted, a second is refused, the status says connecting, and ending
    /// with no session is harmless.
    #[test]
    fn an_attempt_is_minted_once_and_ended_without_a_session() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_client_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        let mut ours = lowlat_credentials {
            size: core::mem::size_of::<lowlat_credentials>() as u32,
            port: 7,
            reserved: 0,
            ufrag: [0; LOWLAT_ICE_MAX],
            pwd: [0; LOWLAT_ICE_MAX],
            fingerprint: [0; LOWLAT_FINGERPRINT_MAX],
            aes256: [0; LOWLAT_ICE_MAX],
        };
        let mut status = lowlat_client_status {
            size: core::mem::size_of::<lowlat_client_status>() as u32,
            state: 99,
            disconnect: 0,
            rtt_ms: 0,
            behind: 0,
            behind_ms: 0,
            pictures: 0,
            skipped: 0,
            audio_packets: 0,
        };
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert_eq!(status.state, LOWLAT_CLIENT_IDLE);

        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    core::ptr::null(),
                    c"one".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_WEB as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_ERR_INVALID_ARGUMENT,
            "the browser pipe was accepted"
        );
        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    core::ptr::null(),
                    c"one".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_OK
        );
        assert_eq!(ours.port, 0, "a client's offer has no port yet");
        assert_eq!(taken(&ours.ufrag).map(str::len), Some(8));
        assert_eq!(taken(&ours.aes256).map(str::len), Some(254));
        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    core::ptr::null(),
                    c"two".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_ERR_ALREADY_STARTED
        );
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert_eq!(status.state, LOWLAT_CLIENT_CONNECTING);
        assert_eq!(
            unsafe { lowlat_client_send_user_data(handle, 1, c"x".as_ptr().cast(), 1) },
            LOWLAT_ERR_NOT_STARTED
        );

        unsafe { lowlat_client_end_connection(handle) };
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert_eq!(status.state, LOWLAT_CLIENT_IDLE);
        let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
        assert_eq!(
            unsafe {
                lowlat_client_poll_events(
                    handle,
                    0,
                    event.as_mut_ptr(),
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                )
            },
            LOWLAT_TIMEOUT,
            "an ending the application caused raised an event"
        );
        unsafe { lowlat_client_destroy(handle) };
    }

    /// The legacy setting empties the offer's media key, which is the whole
    /// of how a client asks for that cipher.
    #[test]
    fn the_legacy_setting_offers_no_media_key() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        assert_eq!(
            unsafe { lowlat_client_create(core::ptr::null(), &raw mut handle) },
            LOWLAT_OK
        );
        let mut cfg = lowlat_client_config {
            size: core::mem::size_of::<lowlat_client_config>() as u32,
            resolution_x: 0,
            resolution_y: 0,
            raw_audio: false,
            legacy_cipher: true,
            shared_address_space: false,
            reserved: 0,
            server_count: 0,
            servers: [[0; LOWLAT_SERVER_MAX]; LOWLAT_SERVERS_MAX],
        };
        let mut ours = lowlat_credentials {
            size: core::mem::size_of::<lowlat_credentials>() as u32,
            port: 0,
            reserved: 0,
            ufrag: [0; LOWLAT_ICE_MAX],
            pwd: [0; LOWLAT_ICE_MAX],
            fingerprint: [0; LOWLAT_FINGERPRINT_MAX],
            aes256: [0; LOWLAT_ICE_MAX],
        };
        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    &raw const cfg,
                    c"old".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_OK
        );
        assert_eq!(taken(&ours.aes256), Some(""));
        assert_eq!(taken(&ours.fingerprint).map(str::len), Some(64));

        // A server that does not resolve is refused while the caller can
        // still fix it.
        unsafe { lowlat_client_end_connection(handle) };
        cfg.server_count = 1;
        put(&mut cfg.servers[0], "no-such-host.invalid:3478");
        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    &raw const cfg,
                    c"old".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        unsafe { lowlat_client_destroy(handle) };
    }
}
