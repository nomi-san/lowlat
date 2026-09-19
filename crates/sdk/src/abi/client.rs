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

use ::lowlat_client::config::{Backend, Decoding, FrameKind};
use ::lowlat_client::{Client, Event, Outcome};
use lowlat_common::events::Delivery;
use lowlat_event_type::*;
use lowlat_outcome::*;

use super::guard;
use super::lowlat_status::{self, *};
use super::shared::*;

/// Which decoder a client is built on.
///
/// **The choice is by index, as the host's encoder is; unset, the first
/// that opens on the device named.** A machine without any is refused at
/// creation with the stage named, exactly as a host without an encoder is.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_decoder {
    LOWLAT_DECODER_AUTO = 0,
    LOWLAT_DECODER_OPEN = 1,
    LOWLAT_DECODER_VENDOR = 2,
    /// No decoder: the session carries control and sound, and every picture
    /// is taken off the wire and dropped. A client with nowhere to draw.
    LOWLAT_DECODER_NONE = 3,
}

/// How pictures leave the library.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_frame_kind {
    /// Planes in memory the library owns for the lease.
    LOWLAT_FRAME_PLANES = 0,
    /// A device-level handle the application imports into its own device.
    /// No decoder exports one yet: refused at creation.
    LOWLAT_FRAME_HANDLE = 1,
}

/// What a client is created with.
///
/// **Zeroed is the sensible default**: the first decoder that opens, planes,
/// the largest picture the generation declares.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_create_info {
    /// Set by the caller to `sizeof(lowlat_client_create_info)`.
    pub size: u32,
    /// One of [`lowlat_decoder`].
    pub decoder: u32,
    /// One of [`lowlat_frame_kind`].
    pub frame_kind: u32,
    /// The largest picture the client takes: what its picture slots are
    /// sized for. Zero for the generation's declared maximum, 4096 square.
    /// Nothing is backed until the first picture is decoded.
    pub max_width: u32,
    pub max_height: u32,
    /// The render node the decoder opens, NUL-terminated; empty for the
    /// first that decodes.
    pub device: [c_char; LOWLAT_OUTPUT_MAX],
}

/// What a client asks of a host, per attempt.
///
/// What the application would like of the picture, for the one stream.
///
/// **Preferences, not requirements.** Each of the three is "this if the host
/// has it": the library masks them with what its decoder was verified to
/// decode before declaring anything, so a stream the decoder cannot take is
/// never asked for, and follows whatever the host then sends. Zeroed is the
/// sensible default and what every established client asks at its defaults.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_video_config {
    /// The picture size asked of the host, or zero for no preference.
    ///
    /// **A request to change the host's display, not a description of this
    /// one.** An established host takes the owner's figure as a mode request,
    /// so set it only to change the person's monitor.
    pub resolution_x: u32,
    pub resolution_y: u32,
    /// The second codec.
    pub hevc: bool,
    /// Ten-bit colour, which implies the second codec.
    pub ten_bit: bool,
    /// Full chroma, which implies the second codec.
    pub chroma_444: bool,
    pub reserved: u8,
}

/// **Zeroed is the sensible default**: no size request, no colour
/// preference, compressed sound, the current cipher, no reflexive servers.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_config {
    /// Set by the caller to `sizeof(lowlat_client_config)`.
    pub size: u32,
    /// The picture: the size asked of the host and the preferences.
    pub video: lowlat_client_video_config,
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
    /// [`LOWLAT_DECODER_NONE_YET`], [`LOWLAT_DECODER_BUILT`] or
    /// [`LOWLAT_DECODER_FAILED`].
    pub decoder: u32,
    /// Pictures decoded and published, not yet taken by the application.
    pub queue_depth: u32,
    /// The last picture's decode and read-back, in microseconds.
    pub decode_us: u32,
    pub readback_us: u32,
    /// Pictures decoded.
    pub decoded: u64,
    /// Bytes taken off the video channel, so a rate can be read as a
    /// difference over time.
    pub video_bytes: u64,
    /// The host's own encode time for the stream, as it last reported it,
    /// in microseconds; zero until it has.
    pub encode_us: u32,
    /// The codec the decoder was built for, one of [`lowlat_codec`]; zero
    /// before a build.
    pub codec: u32,
    /// The decoder backend in use, one of `lowlat_decoder` as resolved at
    /// creation: never `LOWLAT_DECODER_AUTO`.
    pub backend: u32,
    /// Input reports dropped because the session thread was not keeping up.
    /// Nonzero means the loop is not running, not that input is fast.
    pub input_dropped: u32,
    /// Sound packets decoded and handed out by `lowlat_client_acquire_audio`.
    pub audio_decoded: u64,
    /// Sound packets dropped because the application had not taken the
    /// ones before them: the pool holds 32.
    pub audio_dropped: u32,
    /// Sound packets the decoder refused, or that describe a stream this
    /// library does not decode.
    pub audio_refused: u32,
    /// Sound packets taken off the wire and not yet acquired.
    pub audio_queued: u32,
    /// How long the last acquired packet waited between the wire and the
    /// call, in milliseconds.
    pub audio_age_ms: u32,
    /// What the sound decoder was built for: [`LOWLAT_AUDIO_OPUS`],
    /// [`LOWLAT_AUDIO_PCM`], or zero before a build.
    pub audio_codec: u32,
    /// What the client reports to the host every two seconds: the smoothed
    /// decode and hand-over per picture, and the smoothed decode per sound
    /// packet, in microseconds; zero until something has been timed.
    pub decode_reported_us: u32,
    pub audio_reported_us: u32,
    /// The declaration, in the wire's flag bits: what the application asked
    /// (the preferences as flags, unmasked) and what was declared after the
    /// mask; zero before an attempt.
    pub asked_flags: u32,
    pub declared_flags: u32,
    /// The stream as the decoder built it: one of `LOWLAT_FORMAT_*`, or
    /// zero before a build. With `codec`, what the host turned out to send.
    pub stream_format: u32,
}

/// The sound codec on the wire, as `lowlat_client_status.audio_codec`
/// reports it.
pub const LOWLAT_AUDIO_OPUS: u32 = 1;
pub const LOWLAT_AUDIO_PCM: u32 = 2;

/// Modifier bits for [`lowlat_client_send_key`]. The lock bits are the toggles'
/// state, which a host reads to keep its own locks in step.
pub const LOWLAT_MOD_LSHIFT: u32 = 0x0001;
pub const LOWLAT_MOD_RSHIFT: u32 = 0x0002;
pub const LOWLAT_MOD_LCTRL: u32 = 0x0040;
pub const LOWLAT_MOD_RCTRL: u32 = 0x0080;
pub const LOWLAT_MOD_LALT: u32 = 0x0100;
pub const LOWLAT_MOD_RALT: u32 = 0x0200;
pub const LOWLAT_MOD_LGUI: u32 = 0x0400;
pub const LOWLAT_MOD_RGUI: u32 = 0x0800;
pub const LOWLAT_MOD_NUM: u32 = 0x1000;
pub const LOWLAT_MOD_CAPS: u32 = 0x2000;

/// Mouse buttons for [`lowlat_client_send_mouse_button`].
pub const LOWLAT_MOUSE_LEFT: u32 = 1;
pub const LOWLAT_MOUSE_MIDDLE: u32 = 2;
pub const LOWLAT_MOUSE_RIGHT: u32 = 3;
pub const LOWLAT_MOUSE_X1: u32 = 4;
pub const LOWLAT_MOUSE_X2: u32 = 5;

/// Pad buttons for [`lowlat_client_send_pad_button`], by index.
///
/// **Not the bits of [`lowlat_pad_state`]**: the two forms number the
/// buttons differently and neither is derivable from the other.
pub const LOWLAT_PAD_A: u32 = 0;
pub const LOWLAT_PAD_B: u32 = 1;
pub const LOWLAT_PAD_X: u32 = 2;
pub const LOWLAT_PAD_Y: u32 = 3;
pub const LOWLAT_PAD_BACK: u32 = 4;
pub const LOWLAT_PAD_GUIDE: u32 = 5;
pub const LOWLAT_PAD_START: u32 = 6;
pub const LOWLAT_PAD_LSTICK: u32 = 7;
pub const LOWLAT_PAD_RSTICK: u32 = 8;
pub const LOWLAT_PAD_LSHOULDER: u32 = 9;
pub const LOWLAT_PAD_RSHOULDER: u32 = 10;
pub const LOWLAT_PAD_DPAD_UP: u32 = 11;
pub const LOWLAT_PAD_DPAD_DOWN: u32 = 12;
pub const LOWLAT_PAD_DPAD_LEFT: u32 = 13;
pub const LOWLAT_PAD_DPAD_RIGHT: u32 = 14;

/// Pad axes for [`lowlat_client_send_pad_axis`]. Sticks span the signed range;
/// triggers run from zero.
pub const LOWLAT_PAD_AXIS_LX: u32 = 0;
pub const LOWLAT_PAD_AXIS_LY: u32 = 1;
pub const LOWLAT_PAD_AXIS_RX: u32 = 2;
pub const LOWLAT_PAD_AXIS_RY: u32 = 3;
pub const LOWLAT_PAD_AXIS_LT: u32 = 4;
pub const LOWLAT_PAD_AXIS_RT: u32 = 5;

/// Button bits for [`lowlat_pad_state`].
pub const LOWLAT_PAD_STATE_DPAD_UP: u16 = 0x0001;
pub const LOWLAT_PAD_STATE_DPAD_DOWN: u16 = 0x0002;
pub const LOWLAT_PAD_STATE_DPAD_LEFT: u16 = 0x0004;
pub const LOWLAT_PAD_STATE_DPAD_RIGHT: u16 = 0x0008;
pub const LOWLAT_PAD_STATE_START: u16 = 0x0010;
pub const LOWLAT_PAD_STATE_BACK: u16 = 0x0020;
pub const LOWLAT_PAD_STATE_LSTICK: u16 = 0x0040;
pub const LOWLAT_PAD_STATE_RSTICK: u16 = 0x0080;
pub const LOWLAT_PAD_STATE_LSHOULDER: u16 = 0x0100;
pub const LOWLAT_PAD_STATE_RSHOULDER: u16 = 0x0200;
pub const LOWLAT_PAD_STATE_GUIDE: u16 = 0x0400;
pub const LOWLAT_PAD_STATE_TOUCHPAD: u16 = 0x0800;
pub const LOWLAT_PAD_STATE_A: u16 = 0x1000;
pub const LOWLAT_PAD_STATE_B: u16 = 0x2000;
pub const LOWLAT_PAD_STATE_X: u16 = 0x4000;
pub const LOWLAT_PAD_STATE_Y: u16 = 0x8000;

/// A whole pad at one moment, for [`lowlat_client_send_pad_state`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_pad_state {
    /// Set by the caller to `sizeof(lowlat_pad_state)`.
    pub size: u32,
    /// `LOWLAT_PAD_STATE_*` bits.
    pub buttons: u16,
    pub lx: i16,
    pub ly: i16,
    pub rx: i16,
    pub ry: i16,
    pub lt: u8,
    pub rt: u8,
}

/// No decoder has been built yet: no parameter set has arrived.
pub const LOWLAT_DECODER_NONE_YET: u32 = 0;
/// A decoder exists and is being fed.
pub const LOWLAT_DECODER_BUILT: u32 = 1;
/// No decoder can serve the stream; the ended event said so.
pub const LOWLAT_DECODER_FAILED: u32 = 2;

/// One plane of a picture.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_plane {
    /// The first sample of the first row, or null for a plane the layout
    /// does not have.
    pub data: *const u8,
    /// Bytes from one row to the next.
    pub pitch: u32,
}

/// Eight bits: a luma plane and an interleaved chroma plane at half the
/// rows.
pub const LOWLAT_FORMAT_NV12: u32 = 1;
/// Ten bits in sixteen-bit samples, the value in the high bits; the same
/// two planes.
pub const LOWLAT_FORMAT_P010: u32 = 2;
/// Eight bits, full chroma: three planes of the picture's size, luma then
/// the two chroma planes.
pub const LOWLAT_FORMAT_YUV444: u32 = 3;
/// Ten bits in sixteen-bit samples, the value in the high bits; the same
/// three planes.
pub const LOWLAT_FORMAT_YUV444_16: u32 = 4;

/// A decoded picture, lent to the application.
///
/// Valid from the acquire that filled it until the release that names it.
/// Every field the renderer needs is here: nothing is read from the stream.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_frame {
    /// Set by the caller to `sizeof(lowlat_frame)`.
    pub size: u32,
    /// One of [`lowlat_frame_kind`]: what the picture is handed out as.
    pub kind: u32,
    /// One of `LOWLAT_FORMAT_*`.
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// One of [`lowlat_rotation`], applied at present time.
    pub rotation: u32,
    /// The encoder generation the picture belongs to.
    pub generation: u32,
    /// The picture's order in its stream: a later picture has a higher
    /// number, and a gap between two consecutive presents is a skip.
    pub sequence: u64,
    /// Luma, then chroma. A layout with fewer planes leaves the rest null.
    pub planes: [lowlat_plane; 3],
    /// Which slot this is, for the release.
    pub slot: u32,
}

/// A synchronisation object the application's device signals when it has
/// finished reading a picture.
///
/// **None is the only kind in this version**, because every picture leaves
/// as planes that were copied; the shape is fixed so a handle path adds a
/// kind rather than a call.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_fence {
    /// One of [`lowlat_fence_kind`].
    pub kind: u32,
    /// The descriptor or handle, as the kind says.
    pub handle: u64,
    /// The value to wait for, as the kind says.
    pub value: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_fence_kind {
    /// Reusable now.
    LOWLAT_FENCE_NONE = 0,
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
        let info = unsafe { info.as_ref() };
        if let Some(info) = info
            && (info.size as usize) < core::mem::size_of::<lowlat_client_create_info>()
        {
            return LOWLAT_ERR_INVALID_ARGUMENT;
        }
        let decoding = match info {
            None => Decoding::default(),
            Some(info) => {
                let backend = match info.decoder {
                    code if code == lowlat_decoder::LOWLAT_DECODER_AUTO as u32 => Backend::Auto,
                    code if code == lowlat_decoder::LOWLAT_DECODER_OPEN as u32 => Backend::Vaapi,
                    code if code == lowlat_decoder::LOWLAT_DECODER_VENDOR as u32 => Backend::Nvdec,
                    code if code == lowlat_decoder::LOWLAT_DECODER_NONE as u32 => Backend::None,
                    _ => return LOWLAT_ERR_INVALID_ARGUMENT,
                };
                let kind = match info.frame_kind {
                    code if code == lowlat_frame_kind::LOWLAT_FRAME_PLANES as u32 => {
                        FrameKind::Planes
                    }
                    code if code == lowlat_frame_kind::LOWLAT_FRAME_HANDLE as u32 => {
                        FrameKind::Handle
                    }
                    _ => return LOWLAT_ERR_INVALID_ARGUMENT,
                };
                let Some(device) = taken(&info.device) else {
                    return LOWLAT_ERR_INVALID_ARGUMENT;
                };
                Decoding {
                    backend,
                    device: device.to_string(),
                    kind,
                    ceiling: (info.max_width, info.max_height),
                }
            }
        };
        let mut seam = match Client::new(&decoding) {
            Ok(seam) => seam,
            Err(error) => return refused(error),
        };
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
        video: video_of(&cfg.video),
        raw_audio: cfg.raw_audio,
        legacy_cipher: cfg.legacy_cipher,
        servers,
        shared_address_space: cfg.shared_address_space,
    })
}

fn video_of(video: &lowlat_client_video_config) -> ::lowlat_client::config::Video {
    ::lowlat_client::config::Video {
        resolution: (video.resolution_x, video.resolution_y),
        hevc: video.hevc,
        ten_bit: video.ten_bit,
        chroma_444: video.chroma_444,
    }
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
        Error::Decoder(stage) => {
            use ::lowlat_client::seam::DecoderStage;
            match stage {
                DecoderStage::Runtime => LOWLAT_ERR_NO_DECODER_RUNTIME,
                DecoderStage::Device => LOWLAT_ERR_NO_DECODER_DEVICE,
                DecoderStage::Profile => LOWLAT_ERR_NO_DECODER_PROFILE,
                DecoderStage::Unsupported => LOWLAT_ERR_DECODER_UNSUPPORTED,
            }
        }
        Error::TooManyHeld => LOWLAT_ERR_TOO_MANY_HELD,
        Error::NoSession => LOWLAT_ERR_NOT_STARTED,
        Error::TooSmall(_) => LOWLAT_ERR_TOO_SMALL,
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

/// Say where the picture is drawn.
///
/// The rectangle is in the same units as the positions the application
/// reports, and that is all the library knows about the window: no fit is
/// computed here, so stretching, shrinking, a percent scale and a rotated
/// picture are the application's ways of producing one rectangle, and a
/// display scale factor never enters. The picture's own size comes from the
/// stream. A zero rectangle means there is nothing to aim at, and absolute
/// motion is not sent until one is set.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] x The rectangle's left edge in the window.
/// @param[in] y Its top edge.
/// @param[in] w Its width; zero for no picture area.
/// @param[in] h Its height.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_set_viewport(
    cl: *mut lowlat_client,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let viewport = ::lowlat_client::input::Viewport { x, y, w, h };
            if handle.held().seam.set_viewport(viewport) {
                LOWLAT_OK
            } else {
                LOWLAT_ERR_NOT_STARTED
            }
        })
    }
}

/// Change what the application would like of the picture, mid-session.
///
/// The new declaration is masked by capability as at the attempt and
/// restated to the host with a reinitialisation request; the decoder is torn
/// down with it, so the next keyframe builds one for whatever the host now
/// sends. Costs the host one keyframe, and an established host an encoder
/// rebuild, so it is for a person changing a setting rather than a loop. The
/// size request travels with it.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] video The preferences, whole.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_UNKNOWN_ATTEMPT`] with no attempt
/// to apply them to.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `video` points at a readable
/// structure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_set_video_config(
    cl: *mut lowlat_client,
    video: *const lowlat_client_video_config,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            if video.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let video = video_of(&*video);
            match handle.held().seam.set_video(video) {
                Ok(()) => LOWLAT_OK,
                Err(error) => refused(error),
            }
        })
    }
}

/// Hand one report to the session, or say there is none.
fn report(cl: *mut lowlat_client, input: ::lowlat_client::input::Input) -> lowlat_status {
    // SAFETY: every caller is an entry point whose contract is that `cl`
    // came from `lowlat_client_create`.
    unsafe {
        entered(cl, |handle| {
            if handle.held().seam.send_input(input) {
                LOWLAT_OK
            } else {
                LOWLAT_ERR_NOT_STARTED
            }
        })
    }
}

/// A key, by the usage code of the physical key. A code of zero is no key
/// and is not sent.
///
/// The rules every client applies are the library's (docs/10-client.md
/// section 8), here and in the calls below, and **none of them blocks**:
/// reports cross a fixed ring to the session thread, and a ring that fills --
/// a thread that is not running -- drops the newest and counts it in
/// [`lowlat_client_status`].
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] code The usage code.
/// @param[in] mods `LOWLAT_MOD_*` bits in effect, lock state included.
/// @param[in] pressed Down or up.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_key(
    cl: *mut lowlat_client,
    code: u32,
    mods: u32,
    pressed: bool,
) -> lowlat_status {
    report(
        cl,
        ::lowlat_client::input::Input::Key {
            code,
            mods,
            pressed,
        },
    )
}

/// A mouse button, with where the pointer was in the window's units. A press
/// outside the picture's rectangle is not sent; a release always is.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] button One of `LOWLAT_MOUSE_*`.
/// @param[in] pressed Down or up.
/// @param[in] x Where the pointer was.
/// @param[in] y Where the pointer was.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_mouse_button(
    cl: *mut lowlat_client,
    button: u32,
    pressed: bool,
    x: i32,
    y: i32,
) -> lowlat_status {
    report(
        cl,
        ::lowlat_client::input::Input::Button {
            button,
            pressed,
            x,
            y,
        },
    )
}

/// Wheel movement, 120 to a detent, positive away from the hand.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] x Sideways.
/// @param[in] y Up and down.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_mouse_wheel(
    cl: *mut lowlat_client,
    x: i32,
    y: i32,
) -> lowlat_status {
    report(cl, ::lowlat_client::input::Input::Wheel { x, y })
}

/// A pointer position in the window's units, mapped into the picture through
/// the viewport; or a delta when relative, scaled by the picture's size
/// against the viewport's. An absolute position before a viewport is set is
/// not sent.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] x The position, or the delta.
/// @param[in] y The position, or the delta.
/// @param[in] relative A delta rather than a position.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_mouse_motion(
    cl: *mut lowlat_client,
    x: i32,
    y: i32,
    relative: bool,
) -> lowlat_status {
    report(cl, ::lowlat_client::input::Input::Motion { x, y, relative })
}

/// One button of a pad. The pad identifier is the application's and is
/// arbitrary; a host maps it to a slot.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] pad The pad.
/// @param[in] button One of `LOWLAT_PAD_*`, the index form.
/// @param[in] pressed Down or up.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_pad_button(
    cl: *mut lowlat_client,
    pad: u32,
    button: u32,
    pressed: bool,
) -> lowlat_status {
    report(
        cl,
        ::lowlat_client::input::Input::PadButton {
            pad,
            button,
            pressed,
        },
    )
}

/// One axis of a pad.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] pad The pad.
/// @param[in] axis One of `LOWLAT_PAD_AXIS_*`.
/// @param[in] value The position: a stick over the signed range, a trigger from zero.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_pad_axis(
    cl: *mut lowlat_client,
    pad: u32,
    axis: u32,
    value: i16,
) -> lowlat_status {
    report(
        cl,
        ::lowlat_client::input::Input::PadAxis { pad, axis, value },
    )
}

/// A whole pad at once. An unchanged state for the same pad is not sent
/// again.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] pad The pad.
/// @param[in] state The state, its `size` set.
/// @returns [`LOWLAT_OK`], [`LOWLAT_ERR_NOT_STARTED`] with no session up, or
/// [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `state` points to one
/// [`lowlat_pad_state`] whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_pad_state(
    cl: *mut lowlat_client,
    pad: u32,
    state: *const lowlat_pad_state,
) -> lowlat_status {
    let Some(state) = (unsafe { state.as_ref() }) else {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    };
    if (state.size as usize) < core::mem::size_of::<lowlat_pad_state>() {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    }
    report(
        cl,
        ::lowlat_client::input::Input::PadState {
            pad,
            state: ::lowlat_client::input::PadState {
                buttons: state.buttons,
                lx: state.lx,
                ly: state.ly,
                rx: state.rx,
                ry: state.ry,
                lt: state.lt,
                rt: state.rt,
            },
        },
    )
}

/// The pad is gone. The host destroys its device, which releases everything.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] pad The pad.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_pad_unplug(
    cl: *mut lowlat_client,
    pad: u32,
) -> lowlat_status {
    report(cl, ::lowlat_client::input::Input::PadUnplug { pad })
}

/// Everything held comes up on the host; sent on losing focus. Pads are
/// centred by it, not unplugged.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_NOT_STARTED`] with no session up.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_release_all(cl: *mut lowlat_client) -> lowlat_status {
    report(cl, ::lowlat_client::input::Input::ReleaseAll)
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
                decoder: t.decoder.load(Ordering::Relaxed),
                queue_depth: t.queue_depth.load(Ordering::Relaxed),
                decode_us: t.decode_us.load(Ordering::Relaxed),
                readback_us: t.readback_us.load(Ordering::Relaxed),
                decoded: t.decoded.load(Ordering::Relaxed),
                video_bytes: t.video_bytes.load(Ordering::Relaxed),
                encode_us: t.encode_us.load(Ordering::Relaxed),
                codec: t.codec.load(Ordering::Relaxed),
                backend: if held.seam.node().is_some() {
                    lowlat_decoder::LOWLAT_DECODER_OPEN as u32
                } else {
                    lowlat_decoder::LOWLAT_DECODER_NONE as u32
                },
                input_dropped: t.input_dropped.load(Ordering::Relaxed),
                audio_decoded: t.audio_decoded.load(Ordering::Relaxed),
                audio_dropped: t.audio_dropped.load(Ordering::Relaxed),
                audio_refused: t.audio_refused.load(Ordering::Relaxed),
                audio_queued: u32::try_from(held.seam.sound_queued()).unwrap_or(u32::MAX),
                audio_age_ms: t.audio_age_ms.load(Ordering::Relaxed),
                audio_codec: t.audio_codec.load(Ordering::Relaxed),
                decode_reported_us: t.decode_reported_us.load(Ordering::Relaxed),
                audio_reported_us: t.audio_reported_us.load(Ordering::Relaxed),
                asked_flags: t.asked_flags.load(Ordering::Relaxed),
                declared_flags: t.declared_flags.load(Ordering::Relaxed),
                stream_format: t.stream_format.load(Ordering::Relaxed),
            };
            LOWLAT_OK
        })
    }
}

/// Take the newest picture, waiting up to `timeout_ms` for one newer than
/// the last one taken.
///
/// **Acquire is the poll.** Older pictures that were ready are discarded on
/// the way: the newest is what a renderer wants, and a picture it never
/// looked at is the one nothing will miss. A picture stays valid until it is
/// released and may be presented as often as the application likes in
/// between. At most two are held at once -- the one being presented and the
/// one just acquired, so a swap has no gap -- and a third acquire is refused
/// with [`LOWLAT_ERR_TOO_MANY_HELD`] rather than dropping one silently.
///
/// **Outside the handle's lock**, like the event poll: a wait here leaves
/// every other call answerable.
///
/// @param[in] cl The handle.
/// @param[in] stream The stream, zero in this version.
/// @param[in] timeout_ms How long to wait. Zero polls.
/// @param[out] frame The picture, when [`LOWLAT_OK`].
/// @returns [`LOWLAT_OK`], [`LOWLAT_TIMEOUT`] with nothing newer in time,
/// [`LOWLAT_ERR_TOO_MANY_HELD`], [`LOWLAT_ERR_NOT_STARTED`] with no session, or
/// [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `frame` points to one
/// [`lowlat_frame`] whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_acquire_frame(
    cl: *mut lowlat_client,
    stream: u8,
    timeout_ms: u32,
    frame: *mut lowlat_frame,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let Some(frame) = frame.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (frame.size as usize) < core::mem::size_of::<lowlat_frame>() || stream != 0 {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            // The queue and the last sequence are read under the lock and
            // the wait happens outside it.
            let (frames, after, session) = {
                let held = handle.held();
                (
                    std::sync::Arc::clone(held.seam.frames()),
                    held.seam.last_seq(),
                    held.attempt.is_some(),
                )
            };
            if !session {
                return LOWLAT_ERR_NOT_STARTED;
            }
            let taken = match frames.acquire(after, Duration::from_millis(u64::from(timeout_ms))) {
                Ok(Some(taken)) => taken,
                Ok(None) => return LOWLAT_TIMEOUT,
                Err(_) => return LOWLAT_ERR_TOO_MANY_HELD,
            };
            handle.held().seam.set_last_seq(taken.seq);
            *frame = lowlat_frame {
                size: frame.size,
                kind: lowlat_frame_kind::LOWLAT_FRAME_PLANES as u32,
                format: ::lowlat_client::decode::format_code(taken.frame.format),
                width: taken.frame.width,
                height: taken.frame.height,
                rotation: match taken.frame.rotation {
                    ::lowlat_core::video::Rotation::Deg90 => lowlat_rotation::LOWLAT_ROTATION_90,
                    ::lowlat_core::video::Rotation::Deg180 => lowlat_rotation::LOWLAT_ROTATION_180,
                    ::lowlat_core::video::Rotation::Deg270 => lowlat_rotation::LOWLAT_ROTATION_270,
                    _ => lowlat_rotation::LOWLAT_ROTATION_NONE,
                } as u32,
                generation: taken.frame.generation,
                sequence: taken.seq,
                planes: [
                    lowlat_plane {
                        data: taken.y,
                        pitch: u32::try_from(taken.pitch).unwrap_or(u32::MAX),
                    },
                    lowlat_plane {
                        data: taken.uv,
                        pitch: u32::try_from(taken.pitch).unwrap_or(u32::MAX),
                    },
                    lowlat_plane {
                        data: taken.v,
                        pitch: if taken.v.is_null() {
                            0
                        } else {
                            u32::try_from(taken.pitch).unwrap_or(u32::MAX)
                        },
                    },
                ],
                slot: u32::try_from(taken.index).unwrap_or(u32::MAX),
            };
            LOWLAT_OK
        })
    }
}

/// Give a picture back.
///
/// @param[in] cl The handle.
/// @param[in] frame The picture, as acquired.
/// @param[in] done A fence the application's device signals when it has
/// finished reading, or null for reusable now. **Null is the only value this
/// version takes**: every picture leaves as copied planes.
/// @returns [`LOWLAT_OK`] or [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `frame` points to a
/// [`lowlat_frame`] an acquire filled; `done` is null or points to one
/// [`lowlat_fence`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_release_frame(
    cl: *mut lowlat_client,
    frame: *const lowlat_frame,
    done: *const lowlat_fence,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let Some(frame) = frame.as_ref() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if let Some(done) = done.as_ref()
                && done.kind != lowlat_fence_kind::LOWLAT_FENCE_NONE as u32
            {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let frames = std::sync::Arc::clone(handle.held().seam.frames());
            frames.release(frame.slot as usize);
            LOWLAT_OK
        })
    }
}

/// Take the next sound packet, decoded, waiting up to `timeout_ms` for one.
///
/// **One packet a call, in the order the host sent them, and the device
/// paces.** Signed sixteen-bit stereo at 48 kHz, interleaved, as many frames
/// as the packet held (960 for a host sending 20 ms; at most 8000). Nothing
/// here waits for the right moment to hand a packet out: the application
/// queues it on its device, whose own buffer is what turns a stream of
/// packets into continuous sound and absorbs the drift between the host's
/// clock and the device's. Packets the application has not taken wait in a
/// pool of 32; past that the newest is dropped and counted in
/// `lowlat_client_status.audio_dropped`, which is a caller that stopped
/// calling.
///
/// The wait is outside the handle's lock, as the event poll's is. Two
/// threads calling at once take turns, each getting the next packet.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] timeout_ms How long to wait. Zero polls.
/// @param[out] samples Room for `*count` frames of two samples each.
/// @param[in,out] count How many frames there is room for; on return, how
/// many were written, or how many the waiting packet needs.
/// @returns [`LOWLAT_OK`], [`LOWLAT_TIMEOUT`], [`LOWLAT_ERR_TOO_SMALL`] with
/// the need in `*count` and the packet kept for the next call,
/// [`LOWLAT_ERR_NOT_STARTED`] with no session, or
/// [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `samples` points to at least
/// `2 * *count` values; `count` points to one `uint32_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_acquire_audio(
    cl: *mut lowlat_client,
    timeout_ms: u32,
    samples: *mut i16,
    count: *mut u32,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let Some(count) = count.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if samples.is_null() && *count != 0 {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let Some(listening) = handle.held().seam.sound() else {
                return LOWLAT_ERR_NOT_STARTED;
            };
            let room = (*count as usize).saturating_mul(2);
            // SAFETY: the caller promised `2 * *count` values at `samples`,
            // and a null pointer arrives here only with a count of zero.
            let out = if room == 0 {
                &mut [][..]
            } else {
                core::slice::from_raw_parts_mut(samples, room)
            };
            match listening.acquire(Duration::from_millis(u64::from(timeout_ms)), out) {
                Ok(Some(acquired)) => {
                    *count = u32::try_from(acquired.frames).unwrap_or(u32::MAX);
                    LOWLAT_OK
                }
                Ok(None) => {
                    *count = 0;
                    LOWLAT_TIMEOUT
                }
                Err(::lowlat_client::Error::TooSmall(need)) => {
                    *count = u32::try_from(need).unwrap_or(u32::MAX);
                    LOWLAT_ERR_TOO_SMALL
                }
                Err(error) => refused(error),
            }
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
                Outcome::DecoderFailed => (LOWLAT_OUTCOME_DECODER_FAILED, 0),
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
        Event::Relative { relative, x, y } => lowlat_event {
            kind: LOWLAT_EVENT_RELATIVE,
            dropped,
            body: lowlat_event_body {
                relative: lowlat_relative_event {
                    relative: *relative,
                    x: *x,
                    y: *y,
                },
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

    /// A creation with no decoder, which is what a machine without a device
    /// can still do.
    fn no_decoder() -> lowlat_client_create_info {
        lowlat_client_create_info {
            size: core::mem::size_of::<lowlat_client_create_info>() as u32,
            decoder: lowlat_decoder::LOWLAT_DECODER_NONE as u32,
            frame_kind: lowlat_frame_kind::LOWLAT_FRAME_PLANES as u32,
            max_width: 0,
            max_height: 0,
            device: [0; LOWLAT_OUTPUT_MAX],
        }
    }

    /// The whole seam through the boundary against nothing: an attempt is
    /// minted, a second is refused, the status says connecting, and ending
    /// with no session is harmless.
    #[test]
    fn an_attempt_is_minted_once_and_ended_without_a_session() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        let info = no_decoder();
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_OK
        );
        // No session: nothing to acquire from.
        let mut frame = lowlat_frame {
            size: core::mem::size_of::<lowlat_frame>() as u32,
            kind: 0,
            format: 0,
            width: 0,
            height: 0,
            rotation: 0,
            generation: 0,
            sequence: 0,
            planes: [lowlat_plane {
                data: core::ptr::null(),
                pitch: 0,
            }; 3],
            slot: 0,
        };
        assert_eq!(
            unsafe { lowlat_client_acquire_frame(handle, 0, 0, &raw mut frame) },
            LOWLAT_ERR_NOT_STARTED
        );
        let fence = lowlat_fence {
            kind: 7,
            handle: 0,
            value: 0,
        };
        assert_eq!(
            unsafe { lowlat_client_release_frame(handle, &raw const frame, &raw const fence) },
            LOWLAT_ERR_INVALID_ARGUMENT,
            "a fence kind this version does not take was accepted"
        );
        assert_eq!(
            unsafe { lowlat_client_release_frame(handle, &raw const frame, core::ptr::null()) },
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
            decoder: 0,
            queue_depth: 0,
            decode_us: 0,
            readback_us: 0,
            decoded: 0,
            video_bytes: 0,
            encode_us: 0,
            codec: 0,
            backend: 0,
            input_dropped: 0,
            audio_decoded: 0,
            audio_dropped: 0,
            audio_refused: 0,
            audio_queued: 0,
            audio_age_ms: 0,
            audio_codec: 0,
            decode_reported_us: 0,
            audio_reported_us: 0,
            asked_flags: 0,
            declared_flags: 0,
            stream_format: 0,
        };
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert_eq!(status.state, LOWLAT_CLIENT_IDLE);
        let mut samples = [0i16; 4];
        let mut count: u32 = 2;
        assert_eq!(
            unsafe { lowlat_client_acquire_audio(handle, 0, samples.as_mut_ptr(), &raw mut count) },
            LOWLAT_ERR_NOT_STARTED
        );
        assert_eq!(
            unsafe {
                lowlat_client_acquire_audio(handle, 0, samples.as_mut_ptr(), core::ptr::null_mut())
            },
            LOWLAT_ERR_INVALID_ARGUMENT
        );

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
            video: lowlat_client_video_config {
                resolution_x: 0,
                resolution_y: 0,
                hevc: false,
                ten_bit: false,
                chroma_444: false,
                reserved: 0,
            },
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

    /// A decoder that is not built is refused at creation, with the stage,
    /// and so is a frame kind nothing exports.
    #[test]
    fn what_is_not_built_is_refused_at_creation() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        let mut info = no_decoder();
        info.decoder = lowlat_decoder::LOWLAT_DECODER_VENDOR as u32;
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_ERR_DECODER_UNSUPPORTED
        );
        let mut info = no_decoder();
        info.frame_kind = lowlat_frame_kind::LOWLAT_FRAME_HANDLE as u32;
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_ERR_DECODER_UNSUPPORTED
        );
        let mut info = no_decoder();
        info.decoder = 42;
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        assert!(handle.is_null());
    }
}
