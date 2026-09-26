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
/// **The choice is by kind and render node, as the listing reports them;
/// unset, the first that opens on the device named**: the open stack on
/// that node or the first node that decodes, then the vendor's interface on
/// the card behind it or any, then software. A machine without any is
/// refused at creation with the stage named, exactly as a host without an
/// encoder is.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_decoder {
    LOWLAT_DECODER_AUTO = 0,
    LOWLAT_DECODER_OPEN = 1,
    LOWLAT_DECODER_VENDOR = 2,
    /// No decoder: the session carries control and sound, and every picture
    /// is taken off the wire and dropped. A client with nowhere to draw.
    LOWLAT_DECODER_NONE = 3,
    /// The machine's own codec library, loaded at runtime and only when it
    /// answers that it is an LGPL build -- or a GPL one as well, in a library
    /// reporting `LOWLAT_FEATURE_GPL_LIBAVCODEC`; a build that answers
    /// otherwise is refused with [`LOWLAT_ERR_NO_DECODER_LICENCE`]. Looked
    /// for in the environment (`LOWLAT_FFMPEG_DIR`, `LOWLAT_FFMPEG_VERSION`),
    /// in the directory `lowlat_client_create_info.device` names when it
    /// names one, beside the running executable, then the linker's own way;
    /// the highest major of 4 through 9 that opens wins. Planes only.
    LOWLAT_DECODER_SOFTWARE = 4,
}

/// How pictures leave the library.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_frame_kind {
    /// Planes in memory the library owns for the lease.
    LOWLAT_FRAME_PLANES = 0,
    /// A device-level handle the application imports into its own device:
    /// the picture's planes at offsets into it. Only the vendor decoder
    /// exports one, so asking for it settles the decoder on the vendor's
    /// (`LOWLAT_DECODER_AUTO` then means the vendor's on any device), and
    /// the open decoder refuses it at creation with
    /// [`LOWLAT_ERR_DECODER_UNSUPPORTED`].
    LOWLAT_FRAME_HANDLE = 1,
}

/// What a frame of the handle kind carries.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum lowlat_handle_kind {
    /// A frame of the planes kind: no handle.
    LOWLAT_HANDLE_NONE = 0,
    /// An opaque descriptor of the vendor's compute runtime, which the
    /// same vendor's GL imports as `GL_HANDLE_TYPE_OPAQUE_FD_EXT` and
    /// Vulkan as `VK_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD_BIT`; the
    /// picture's rows are laid out plainly at each plane's offset and
    /// pitch. The descriptor is **the library's for the lease** and is
    /// closed when the allocation behind it is freed, which is after the
    /// last hold on it is released; an import that takes ownership of the
    /// descriptor it is given (GL's does) is given a duplicate.
    LOWLAT_HANDLE_OPAQUE_FD = 1,
    /// Reserved: a buffer descriptor with a layout modifier.
    LOWLAT_HANDLE_DMABUF = 2,
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
    /// first that decodes. For the software decoder, the directory its
    /// library pair is taken from, or empty for the search of its own.
    pub device: [c_char; LOWLAT_OUTPUT_MAX],
}

/// The longest name a decoder's row carries.
pub const LOWLAT_DECODER_NAME_MAX: usize = 128;

/// One slot of the decoder table, as [`lowlat_enum_decoders`] reports it:
/// what creation takes to open exactly this one, and what it decodes --
/// or, with `available` clear, why nothing opens there.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_decoder_info {
    /// Set by the caller to `sizeof(lowlat_decoder_info)`.
    pub size: u32,
    /// Its slot in the table, the `index` it was asked for.
    pub index: u32,
    /// One of [`lowlat_decoder`], `LOWLAT_DECODER_OPEN`,
    /// `LOWLAT_DECODER_VENDOR` or `LOWLAT_DECODER_SOFTWARE`: what
    /// `lowlat_client_create_info.decoder` names to open this one.
    pub decoder: u32,
    /// The largest coded picture per codec, as the device reports it; zero
    /// where it does not say.
    pub max_width_h264: u32,
    pub max_height_h264: u32,
    pub max_width_hevc: u32,
    pub max_height_hevc: u32,
    /// What it decodes. A preference in `lowlat_client_video_config` past
    /// these is masked before anything is declared.
    pub h264: bool,
    pub hevc: bool,
    pub hevc_10: bool,
    pub hevc_444: bool,
    pub hevc_444_10: bool,
    /// Whether it hands pictures out as a handle: what
    /// `lowlat_client_create_info.frame_kind = LOWLAT_FRAME_HANDLE` needs.
    pub handle: bool,
    /// Whether a decoder opened in this slot (minor 12). Clear, every
    /// capability above is false and `driver` says why: a node that is not
    /// there, a device ordinal past the last, a codec library that is not
    /// one this build loads. A loop skips such rows.
    pub available: bool,
    pub reserved: [u8; 1],
    /// The render node, NUL-terminated, for `lowlat_client_create_info
    /// .device`; empty for the vendor's device when no node names it, which
    /// creation takes as the first device. For the software row, the
    /// directory the library pair was found in, or empty for the linker's
    /// own search.
    pub device: [c_char; LOWLAT_OUTPUT_MAX],
    /// A label for a menu, NUL-terminated: the interface, and the card's
    /// maker in brackets where it is known -- `VA-API [Intel]`, `VA-API
    /// [AMD]`, `NVDEC [NVIDIA]`, `libavcodec [LGPL]`; the interface alone
    /// for a slot with nothing behind it.
    pub name: [c_char; LOWLAT_DECODER_NAME_MAX],
    /// The driver's own words, NUL-terminated (minor 13): its banner and
    /// version for the open decoder, the device's product name for the
    /// vendor's, the library's version and licence for software; for a slot
    /// that is not available, why not. Filled only when `size` reaches it.
    pub driver: [c_char; LOWLAT_DECODER_NAME_MAX],
}

/// The row's size before `driver` was appended: the least a caller may
/// pass, and what a caller built against an older header passes.
const DECODER_INFO_MINOR_12: usize = core::mem::offset_of!(lowlat_decoder_info, driver);

/// Slot `index` of the decoder table. **The table is fixed and each call
/// probes one slot** (minor 12): on Linux, slots 0 to 7 are the open
/// decoder on render nodes `renderD128` to `renderD135`, 8 to 15 the
/// vendor's on its devices by ordinal, 16 the software decoder from the
/// codec library's own search. The same slot means the same thing on every
/// machine and every call, and nothing is remembered between calls: each
/// call opens its one slot the way creation opens it and closes it again,
/// so a loop from zero until false costs every slot once -- a few
/// milliseconds for most, some hundred and fifty for a vendor device whose
/// five capabilities are each proved by a real decoder. A slot with nothing
/// usable behind it still answers true, with `available` clear and the
/// reason in `driver`; the loop skips it. For a startup or a settings screen,
/// not a per-frame call.
///
/// An available row is opened by creation with its `decoder` and `device`,
/// and `frame_kind = LOWLAT_FRAME_HANDLE` on a row whose `handle` is set.
///
/// @param[in] index The slot, from zero.
/// @param[out] out One [`lowlat_decoder_info`] with `size` set, filled as
/// far as `size` reaches when there is a slot at `index`: a caller built
/// against an older header gets the fields it knows.
/// @returns True with `out` filled; false past the table's end, or when
/// `out` is null or its `size` is shorter than the row ever was.
///
/// # Safety
///
/// `out` is null or points to one [`lowlat_decoder_info`] whose `size` is
/// set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_enum_decoders(index: u32, out: *mut lowlat_decoder_info) -> bool {
    guard(false, || {
        // SAFETY: the caller's contract.
        let Some(out) = (unsafe { out.as_mut() }) else {
            return false;
        };
        let size = out.size as usize;
        if size < DECODER_INFO_MINOR_12 {
            return false;
        }
        let Some(row) = ::lowlat_client::enumerate::probe(index) else {
            return false;
        };
        let mut info = lowlat_decoder_info {
            size: out.size,
            index,
            decoder: match row.backend {
                Backend::Nvdec => lowlat_decoder::LOWLAT_DECODER_VENDOR,
                Backend::Software => lowlat_decoder::LOWLAT_DECODER_SOFTWARE,
                _ => lowlat_decoder::LOWLAT_DECODER_OPEN,
            } as u32,
            max_width_h264: row.max_h264.0,
            max_height_h264: row.max_h264.1,
            max_width_hevc: row.max_hevc.0,
            max_height_hevc: row.max_hevc.1,
            h264: row.caps.h264,
            hevc: row.caps.hevc,
            hevc_10: row.caps.hevc_10,
            hevc_444: row.caps.hevc_444,
            hevc_444_10: row.caps.hevc_444_10,
            handle: row.handle,
            available: row.available,
            reserved: [0; 1],
            device: [0; LOWLAT_OUTPUT_MAX],
            name: [0; LOWLAT_DECODER_NAME_MAX],
            driver: [0; LOWLAT_DECODER_NAME_MAX],
        };
        put(&mut info.device, &row.device);
        put(&mut info.name, &row.name);
        put(&mut info.driver, &row.driver);
        // As much of the row as the caller's size reaches, and no more:
        // what lies past the caller's structure is the caller's.
        let bytes = size.min(core::mem::size_of::<lowlat_decoder_info>());
        // SAFETY: `out` is a live structure of at least `bytes` bytes by the
        // caller's contract, `info` a whole one, both plain data.
        unsafe {
            core::ptr::copy_nonoverlapping(
                (&raw const info).cast::<u8>(),
                core::ptr::from_mut(out).cast::<u8>(),
                bytes,
            );
        }
        true
    })
}

/// What a client asks of a host, per attempt.
///
/// What the application would like of the picture, for the one stream.
///
/// **Preferences, not requirements.** Each is "this if the host has it": the
/// library masks the codec and the two colour axes with what its decoder was
/// verified to decode before declaring anything, so a stream the decoder
/// cannot take is never asked for, and follows whatever the host then sends.
/// Zeroed is the sensible default: H.264 in the video range.
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
    /// The application's renderer takes the full range (minor 17): its
    /// conversion reads `lowlat_frame.full_range`, so a host may send samples
    /// spanning the whole of their depth. Declared as asked, never masked by
    /// the decoder, which decodes either range alike. False asks for the
    /// video range, which a renderer that assumes it draws right; a caller
    /// built against minor 16 or earlier passed zero here, and gets that.
    pub full_range: bool,
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
    /// The relay to go through (minor 14), as `host:port`, NUL-terminated;
    /// empty for a direct attempt. **Set, the attempt is a relay attempt**:
    /// it offers the relayed address and nothing else, asks no reflexive
    /// server, and every check goes through the relay. Read only when `size`
    /// reaches it.
    pub relay: [c_char; LOWLAT_SERVER_MAX],
    /// The relay's credential (minor 14), NUL-terminated, both required
    /// with a relay. Never logged, and every copy the library takes is
    /// cleared when it is done with it.
    pub relay_username: [c_char; LOWLAT_RELAY_CREDENTIAL_MAX],
    pub relay_password: [c_char; LOWLAT_RELAY_CREDENTIAL_MAX],
}

/// The longest relay username or password this boundary carries.
pub const LOWLAT_RELAY_CREDENTIAL_MAX: usize = 128;

/// The configuration's size before the relay was appended: the least a
/// caller may pass, and what a caller built against an older header passes.
const CONFIG_MINOR_13: usize = core::mem::offset_of!(lowlat_client_config, relay);

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
    /// creation or by `lowlat_client_set_decoder`: never
    /// `LOWLAT_DECODER_AUTO`.
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
    /// This client's number on the host's roster, which is how it finds
    /// itself in the guest list; zero until the host has sent one.
    pub number: u32,
    /// The host's pointer: pictures delivered, names this client no longer
    /// held, and pictures the reader refused.
    pub cursor_images: u32,
    pub cursor_misses: u32,
    pub cursor_refused: u32,
    /// A pad's own reports (minor 10): handed to the session thread, received
    /// from the host, and received for a pad this client never sent as
    /// reports, which are dropped.
    pub pad_reports_sent: u32,
    pub pad_reports_received: u32,
    pub pad_reports_dropped: u32,
    /// The relayed address a relay attempt offered, NUL-terminated, and its
    /// port (minor 14): empty until the relay has one, and for a direct
    /// attempt. Filled only when `size` reaches it.
    pub relay_address: [c_char; LOWLAT_ADDRESS_MAX],
    pub relay_port: u16,
    /// Whether the path goes through the relay (minor 14).
    pub relayed: bool,
    pub reserved: u8,
}

/// The status's size before the relay's fields were appended: the least a
/// caller may pass, and what a caller built against an older header passes.
const STATUS_MINOR_13: usize = core::mem::offset_of!(lowlat_client_status, relay_address);

/// What one channel did, seen from the receiving end.
///
/// **A receiver's figures under a receiver's names.** The host's structure
/// describes a sender -- what it put on the wire, what it resent, its
/// congestion -- and none of that can be measured here; what can is below.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct lowlat_client_channel_metrics {
    /// Fragments accepted: first arrivals, never duplicates.
    pub fragments: u64,
    /// Of those, the ones that arrived behind a later fragment, which on this
    /// transport is a retransmission, or a reorder on a path that reorders.
    pub late: u64,
    /// Fragments refused because they were already here, or already taken.
    pub duplicates: u64,
    /// Fragments refused because they were further ahead than the ring holds.
    pub out_of_window: u64,
    /// Acknowledgements sent with the negative bit, naming this channel: what
    /// the host's fast retransmissions to this client answer.
    pub nacks_sent: u64,
    /// Bytes and messages taken off the channel, so a rate is a difference
    /// over time on the application's clock.
    pub bytes: u64,
    pub messages: u64,
    /// Late arrivals over arrivals, per one-second sample, averaged with a
    /// thirtieth's weight on the newest: the loss the path showed over about
    /// the last thirty seconds, 0 to 1.
    pub loss_30s: f32,
    pub reserved: u32,
}

/// The client's own figures ([`lowlat_client_get_metrics`]).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct lowlat_client_metrics {
    /// Set by the caller to `sizeof(lowlat_client_metrics)`.
    pub size: u32,
    /// How long the session has been established, in milliseconds.
    pub connected_ms: u32,
    /// The smoothed round trip to the host, in milliseconds.
    pub rtt_ms: u32,
    pub reserved: u32,
    pub control: lowlat_client_channel_metrics,
    pub video: lowlat_client_channel_metrics,
    pub audio: lowlat_client_channel_metrics,
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
    /// does not have -- and null for every plane of a frame of the handle
    /// kind, whose planes are `offset` into the handle instead.
    pub data: *const u8,
    /// Bytes from one row to the next.
    pub pitch: u32,
    /// Bytes from the start of the handle to the first sample of the
    /// first row, for a frame of the handle kind; zero otherwise.
    pub offset: u64,
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
    /// One of [`lowlat_handle_kind`]: none for a frame of the planes kind.
    pub handle_kind: u32,
    /// The descriptor, for [`lowlat_handle_kind::LOWLAT_HANDLE_OPAQUE_FD`];
    /// negative otherwise.
    pub fd: i32,
    /// The allocation's ordinal since creation, from one, for a frame of
    /// the handle kind. Descriptor numbers are reused once closed, so this
    /// is what tells one allocation from the next: two frames with the
    /// same number share an import, a new number is a new import, and an
    /// import whose number no longer appears may be dropped.
    pub allocation: u32,
    /// The whole allocation behind the descriptor in bytes, which is what
    /// an import is told; zero for a frame of the planes kind.
    pub handle_size: u64,
    /// The layout modifier, for a kind that has one; zero otherwise.
    pub modifier: u64,
    /// The samples span the whole range of their depth (0 to 255 at eight
    /// bits) rather than the video range (16 to 235 for luma, 16 to 240 for
    /// chroma), as the stream's own parameter set says (minor 15). Nothing
    /// is converted, so a renderer takes this into its conversion: one that
    /// assumes the video range shows a full-range picture darker, its
    /// blacks crushed and its contrast raised. Filled only when `size`
    /// reaches it.
    pub full_range: bool,
    /// When the message the picture was decoded from was taken off the
    /// network, in microseconds of `CLOCK_MONOTONIC`, the clock an
    /// application reads by that name, or of `QueryPerformanceCounter` on
    /// Windows; zero where it is not known (minor 16). Against a reading of
    /// that clock at acquire it is the picture's time in the library, and
    /// after a present its time to the screen.
    /// Filled only when `size` reaches it.
    pub arrived_us: u64,
}

/// The frame's size before `full_range` was appended: the least a caller
/// may pass, and what a caller built against an older header passes.
const FRAME_MINOR_14: usize = core::mem::offset_of!(lowlat_frame, full_range);

/// A synchronisation object the application's device signals when it has
/// finished reading a picture.
///
/// **None is the only kind in this version**: a picture of the planes kind
/// was copied, and one of the handle kind was copied on the device before
/// the acquire returned, so either is reusable once released. The shape is
/// fixed so a kind that needs one adds a kind rather than a call.
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
    /// The pointer's picture, decoded here for the application: one buffer,
    /// grown to the largest picture seen, lent until the next poll.
    cursor: std::sync::Mutex<Vec<u8>>,
    /// The last pad report delivered, lent the same way.
    pad_report: std::sync::Mutex<[u8; ::lowlat_core::pad::REPORT_MAX]>,
    /// Held for the handle's life and declared last, so it is released only
    /// after every thread the handle ran has been joined.
    _timer: lowlat_common::clock::TimerResolution,
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
                    code if code == lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32 => {
                        Backend::Software
                    }
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
            cursor: std::sync::Mutex::new(Vec::new()),
            pad_report: std::sync::Mutex::new([0; ::lowlat_core::pad::REPORT_MAX]),
            _timer: lowlat_common::clock::TimerResolution::raise(),
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
    // A relay needs a credential, and one whose name resolves: the first
    // IPv4 address it has, the family most paths carry.
    let relay = match taken(&cfg.relay)? {
        "" => None,
        text => {
            let found = ::lowlat_net::addrs::resolve_server(text);
            let server = found
                .iter()
                .find(|addr| addr.is_ipv4())
                .or(found.first())
                .copied()?;
            let username = taken(&cfg.relay_username).filter(|text| !text.is_empty())?;
            let password = taken(&cfg.relay_password).filter(|text| !text.is_empty())?;
            Some(::lowlat_client::config::Relay::new(
                server, username, password,
            ))
        }
    };
    Some(::lowlat_client::Config {
        video: video_of(&cfg.video),
        raw_audio: cfg.raw_audio,
        legacy_cipher: cfg.legacy_cipher,
        servers,
        shared_address_space: cfg.shared_address_space,
        relay,
    })
}

fn video_of(video: &lowlat_client_video_config) -> ::lowlat_client::config::Video {
    ::lowlat_client::config::Video {
        resolution: (video.resolution_x, video.resolution_y),
        hevc: video.hevc,
        ten_bit: video.ten_bit,
        chroma_444: video.chroma_444,
        full_range: video.full_range,
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
            use ::lowlat_client::event::DecoderStage;
            match stage {
                DecoderStage::Runtime => LOWLAT_ERR_NO_DECODER_RUNTIME,
                DecoderStage::Device => LOWLAT_ERR_NO_DECODER_DEVICE,
                DecoderStage::Profile => LOWLAT_ERR_NO_DECODER_PROFILE,
                DecoderStage::Unsupported => LOWLAT_ERR_DECODER_UNSUPPORTED,
                DecoderStage::Licence => LOWLAT_ERR_NO_DECODER_LICENCE,
            }
        }
        Error::TooManyHeld => LOWLAT_ERR_TOO_MANY_HELD,
        Error::NoSession => LOWLAT_ERR_NOT_STARTED,
        Error::TooSmall(_) => LOWLAT_ERR_TOO_SMALL,
        Error::Report | Error::PadFamily => LOWLAT_ERR_INVALID_ARGUMENT,
    }
}

/// Begin an attempt: mint the credentials the offer carries.
///
/// **Nothing is sent and no socket is opened.** The application puts what
/// comes back into its offer over its own signaling and calls
/// [`lowlat_client_begin_p2p`] with the answer. One attempt at a time; a second
/// while one exists, or while an ended one is still leaving on another thread,
/// is refused with [`LOWLAT_ERR_ALREADY_STARTED`].
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
            let config = if cfg.is_null() {
                ::lowlat_client::Config::default()
            } else {
                // As far as the caller's size reaches: a caller built against
                // an older header passed less, and what lies past its
                // structure is its own. The rest reads as zero, which is a
                // direct attempt.
                let size = core::ptr::addr_of!((*cfg).size).read_unaligned() as usize;
                if size < CONFIG_MINOR_13 {
                    return LOWLAT_ERR_INVALID_ARGUMENT;
                }
                // SAFETY: every field is plain data for which zero is valid.
                let mut copy: lowlat_client_config = core::mem::zeroed();
                // SAFETY: the caller's structure is at least `size` bytes by
                // its contract; the copy is a whole one, both plain data.
                core::ptr::copy_nonoverlapping(
                    cfg.cast::<u8>(),
                    (&raw mut copy).cast::<u8>(),
                    size.min(core::mem::size_of::<lowlat_client_config>()),
                );
                let config = configured(&copy);
                zeroize::Zeroize::zeroize(&mut copy.relay_username);
                zeroize::Zeroize::zeroize(&mut copy.relay_password);
                match config {
                    Some(config) => config,
                    None => return LOWLAT_ERR_INVALID_ARGUMENT,
                }
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
/// **The wait holds nothing else up.** Calls made on other threads while this
/// one waits are answered at once, as for a handle with no attempt, and a new
/// attempt is refused with [`LOWLAT_ERR_ALREADY_STARTED`] until this returns.
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
            // Taken out under the lock, left outside it: the departure's grace
            // and the joins are what take the time.
            let leaving = {
                let mut held = handle.held();
                let attempt = held.attempt.take();
                attempt.and_then(|attempt| held.seam.detach(&attempt))
            };
            if let Some(leaving) = leaving {
                leaving.finish();
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

/// Choose another decoder, before a session or during one.
///
/// The kind and render node are those of creation and of the listing's rows
/// (`LOWLAT_DECODER_AUTO` walks the automatic order again). The decoder is
/// probed here, on the caller's thread, exactly as creation probes it; a
/// kind that does not open answers with its stage -- `LOWLAT_ERR_NO_DECODER_
/// RUNTIME`, `_DEVICE`, `_PROFILE` or `_LICENCE` -- and **nothing changes**,
/// the running decoder keeps decoding. Before an attempt the choice is
/// replaced and that is all. During a session it is one act: the
/// declaration re-masked by the new decoder's capability and restated to
/// the host where it changed, the running decoder torn down, the new one
/// opened, and one keyframe request with the reinitialisation argument once
/// the new decoder can take one, so the picture resumes at the next
/// keyframe; a picture the application holds stays valid, the queue never
/// closes. Costs the host one keyframe, and an established host an encoder
/// rebuild, so it is for a person changing a setting rather than a loop.
///
/// **The frame kind stays the creation's**: a session created with
/// `LOWLAT_FRAME_HANDLE` refuses this with [`LOWLAT_ERR_DECODER_UNSUPPORTED`],
/// because its device slots are bound to the device; changing that is a
/// recreate. `LOWLAT_DECODER_NONE` is refused the same way.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] decoder One of [`lowlat_decoder`], not `LOWLAT_DECODER_NONE`.
/// @param[in] device The render node, or the software decoder's directory,
/// NUL-terminated; null or empty for the first that decodes.
/// @returns [`LOWLAT_OK`], [`LOWLAT_ERR_INVALID_ARGUMENT`] for a value that
/// is not a decoder, [`LOWLAT_ERR_DECODER_UNSUPPORTED`] for a handle session
/// or `LOWLAT_DECODER_NONE`, or the stage the probe stopped at.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `device` is null or points at a
/// NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_set_decoder(
    cl: *mut lowlat_client,
    decoder: u32,
    device: *const c_char,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let backend = match decoder {
                code if code == lowlat_decoder::LOWLAT_DECODER_AUTO as u32 => Backend::Auto,
                code if code == lowlat_decoder::LOWLAT_DECODER_OPEN as u32 => Backend::Vaapi,
                code if code == lowlat_decoder::LOWLAT_DECODER_VENDOR as u32 => Backend::Nvdec,
                code if code == lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32 => Backend::Software,
                code if code == lowlat_decoder::LOWLAT_DECODER_NONE as u32 => Backend::None,
                _ => return LOWLAT_ERR_INVALID_ARGUMENT,
            };
            let device = if device.is_null() {
                ""
            } else {
                match core::ffi::CStr::from_ptr(device).to_str() {
                    Ok(text) => text,
                    Err(_) => return LOWLAT_ERR_INVALID_ARGUMENT,
                }
            };
            match handle.held().seam.set_decoder(backend, device) {
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
        entered(cl, |handle| match handle.held().seam.send_input(input) {
            Ok(()) => LOWLAT_OK,
            Err(error) => refused(error),
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
/// the viewport; or a delta when relative, sent as the device reported it and
/// not scaled by the size the picture is drawn at, so the host's pointer
/// moves as far as a mouse of its own would. An absolute position before a
/// viewport is set is not sent.
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

/// A DualShock 4's or a DualSense's own report, as the pad delivered it
/// (docs/10-client.md section 8; minor 10).
///
/// The library sends the report raw and the standard state it implies
/// beside it, so a host that does not read reports still has a pad; it
/// normalises a wireless pad's framing on the way in and puts it back on
/// what comes back ([`lowlat_pad_report_event`]). `kind` is
/// `LOWLAT_PAD_REPORT_INPUT` for an input report -- the 64 bytes a USB pad
/// delivers, or the 78 a wireless one does -- or `LOWLAT_PAD_REPORT_FEATURE`
/// for one of the two feature reports the host's driver asks a new device
/// for, read from the pad and sent **before the first input report**:
/// calibration (`0x02` for a DualShock 4 over USB, `0x05` over Bluetooth
/// and for a DualSense) and firmware (`0xA3`, `0x20`), identifier in byte
/// 0. Any subset; the host keeps a default for what did not arrive. The
/// pairing report is the host's and is refused.
///
/// A pad reported here is one product until [`lowlat_client_send_pad_unplug`],
/// and `lowlat_client_send_pad_state`, `_button` and `_axis` are refused for
/// it -- the report already carries what they would say.
///
/// **Keep the identifier below 256.** An established host keys the standard
/// pad messages on its low eight bits and the report on the whole of it, so
/// a wider identifier's reports never reach the pad the states made, and its
/// rumble comes back under the eight-bit one (docs/01-protocol.md 11.1).
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[in] pad The pad, the application's own identifier, below 256.
/// @param[in] type_ One of [`lowlat_pad_type`].
/// @param[in] kind `LOWLAT_PAD_REPORT_INPUT` or `LOWLAT_PAD_REPORT_FEATURE`.
/// @param[in] report The report's bytes, identifier byte first.
/// @param[in] len How many, at most [`LOWLAT_PAD_REPORT_MAX`].
/// @returns [`LOWLAT_OK`]; [`LOWLAT_ERR_NOT_STARTED`] with no session up;
/// [`LOWLAT_ERR_INVALID_ARGUMENT`] for a report this path does not carry
/// (the wrong length or identifier for the product, a feature report other
/// than the two, a wireless checksum that does not verify), for a kind that
/// is not sent, or for a pad already sent as states.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `report` points to `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_send_pad_report(
    cl: *mut lowlat_client,
    pad: u32,
    type_: u32,
    kind: u32,
    report: *const u8,
    len: u32,
) -> lowlat_status {
    use ::lowlat_client::input::ReportKind;
    use ::lowlat_core::pad::Product;
    let product = match type_ {
        x if x == lowlat_pad_type::LOWLAT_PAD_TYPE_DS4 as u32 => Product::DualShock4,
        x if x == lowlat_pad_type::LOWLAT_PAD_TYPE_DS5 as u32 => Product::DualSense,
        _ => return LOWLAT_ERR_INVALID_ARGUMENT,
    };
    let kind = match kind {
        x if x == lowlat_pad_report::LOWLAT_PAD_REPORT_INPUT as u32 => ReportKind::Input,
        x if x == lowlat_pad_report::LOWLAT_PAD_REPORT_FEATURE as u32 => ReportKind::Feature,
        _ => return LOWLAT_ERR_INVALID_ARGUMENT,
    };
    if report.is_null() || len == 0 || len > LOWLAT_PAD_REPORT_MAX {
        return LOWLAT_ERR_INVALID_ARGUMENT;
    }
    // SAFETY: the contract is that `report` points to `len` bytes, and `len`
    // was bounded above.
    let bytes = unsafe { core::slice::from_raw_parts(report, len as usize) };
    // SAFETY: `cl` came from `lowlat_client_create`.
    unsafe {
        entered(cl, |handle| {
            match handle
                .held()
                .seam
                .send_pad_report(pad, product, kind, bytes)
            {
                Ok(()) => LOWLAT_OK,
                Err(error) => refused(error),
            }
        })
    }
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
            if out.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let size = core::ptr::addr_of!((*out).size).read_unaligned();
            if (size as usize) < STATUS_MINOR_13 {
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
            let mut status = lowlat_client_status {
                size,
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
                backend: match held
                    .seam
                    .opened()
                    .map(::lowlat_client::seam::Opened::backend)
                {
                    Some(Backend::Vaapi) => lowlat_decoder::LOWLAT_DECODER_OPEN as u32,
                    Some(Backend::Nvdec) => lowlat_decoder::LOWLAT_DECODER_VENDOR as u32,
                    Some(Backend::Software) => lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32,
                    Some(Backend::Auto | Backend::None) | None => {
                        lowlat_decoder::LOWLAT_DECODER_NONE as u32
                    }
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
                number: t.number.load(Ordering::Relaxed),
                cursor_images: t.cursor_images.load(Ordering::Relaxed),
                cursor_misses: t.cursor_misses.load(Ordering::Relaxed),
                cursor_refused: t.cursor_refused.load(Ordering::Relaxed),
                pad_reports_sent: t.pad_reports_sent.load(Ordering::Relaxed),
                pad_reports_received: t.pad_reports_received.load(Ordering::Relaxed),
                pad_reports_dropped: t.pad_reports_dropped.load(Ordering::Relaxed),
                relay_address: [0; LOWLAT_ADDRESS_MAX],
                relay_port: 0,
                relayed: t.path_relayed.load(Ordering::Relaxed),
                reserved: 0,
            };
            if let Some(relayed) =
                ::lowlat_client::driver::unpack_relayed(t.relayed.load(Ordering::Relaxed))
            {
                put_address(
                    &mut status.relay_address,
                    &mut status.relay_port,
                    &std::net::SocketAddr::V4(relayed),
                );
            }
            // As much as the caller's size reaches, and no more: a caller
            // built against an older header gets the fields it knows.
            // SAFETY: `out` is at least `size` bytes by the caller's
            // contract; `status` is a whole one, both plain data.
            core::ptr::copy_nonoverlapping(
                (&raw const status).cast::<u8>(),
                out.cast::<u8>(),
                (size as usize).min(core::mem::size_of::<lowlat_client_status>()),
            );
            LOWLAT_OK
        })
    }
}

/// One channel's figures, as the session thread last published them.
fn channel_metrics(
    t: &::lowlat_client::Telemetry,
    channel: usize,
) -> lowlat_client_channel_metrics {
    let load = |slot: &[core::sync::atomic::AtomicU64; ::lowlat_client::driver::CHANNELS]| {
        slot.get(channel)
            .map_or(0, |cell| cell.load(Ordering::Relaxed))
    };
    lowlat_client_channel_metrics {
        fragments: load(&t.fragments),
        late: load(&t.late),
        duplicates: load(&t.duplicates),
        out_of_window: load(&t.out_of_window),
        nacks_sent: load(&t.nacks_sent),
        bytes: load(&t.bytes),
        messages: load(&t.messages),
        loss_30s: t
            .loss_30s
            .get(channel)
            .map_or(0.0, |cell| f32::from_bits(cell.load(Ordering::Relaxed))),
        reserved: 0,
    }
}

/// Read what this client measured of the session, per channel.
///
/// **The receiver's figures.** The host's own figures for this guest -- what
/// it sent, what it resent, its rate and round trip -- arrive in the guest
/// list ([`LOWLAT_EVENT_GUEST_LIST`]) for the application to read; this call
/// is the other end of the same path, measured where it can be.
///
/// @param[in] cl The handle from [`lowlat_client_create`].
/// @param[out] out One [`lowlat_client_metrics`] with `size` set, filled.
/// @returns [`LOWLAT_OK`], or [`LOWLAT_ERR_INVALID_ARGUMENT`].
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `out` points to one
/// [`lowlat_client_metrics`] whose `size` is set.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_get_metrics(
    cl: *mut lowlat_client,
    out: *mut lowlat_client_metrics,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            let Some(out) = out.as_mut() else {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            };
            if (out.size as usize) < core::mem::size_of::<lowlat_client_metrics>() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            let held = handle.held();
            let t = held.seam.telemetry();
            *out = lowlat_client_metrics {
                size: out.size,
                connected_ms: t.connected_ms.load(Ordering::Relaxed),
                rtt_ms: t.rtt_ms.load(Ordering::Relaxed),
                reserved: 0,
                control: channel_metrics(t, 0),
                video: channel_metrics(t, 1),
                audio: channel_metrics(t, 2),
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
/// @param[out] frame The picture, when [`LOWLAT_OK`]: one [`lowlat_frame`]
/// with `size` set, filled as far as `size` reaches, so a caller built
/// against an older header gets the fields it knows.
/// @returns [`LOWLAT_OK`], [`LOWLAT_TIMEOUT`] with nothing newer in time,
/// [`LOWLAT_ERR_TOO_MANY_HELD`], [`LOWLAT_ERR_NOT_STARTED`] with no session, or
/// [`LOWLAT_ERR_INVALID_ARGUMENT`], which a `size` shorter than the structure
/// had at minor 14 is too.
///
/// # Safety
///
/// `cl` came from [`lowlat_client_create`]; `frame` points to a
/// [`lowlat_frame`] of at least the `size` it states.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lowlat_client_acquire_frame(
    cl: *mut lowlat_client,
    stream: u8,
    timeout_ms: u32,
    frame: *mut lowlat_frame,
) -> lowlat_status {
    unsafe {
        entered(cl, |handle| {
            if frame.is_null() {
                return LOWLAT_ERR_INVALID_ARGUMENT;
            }
            // Read through the pointer, never a reference to the whole: a
            // caller built against an older header passes a shorter one.
            let size = core::ptr::addr_of!((*frame).size).read_unaligned();
            if (size as usize) < FRAME_MINOR_14 || stream != 0 {
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
            let arrived_us = {
                let mut held = handle.held();
                held.seam.set_last_seq(taken.seq);
                taken
                    .frame
                    .arrived
                    .map_or(0, |stamp| held.seam.arrived_us(stamp))
            };
            let pitch = u32::try_from(taken.pitch).unwrap_or(u32::MAX);
            let full_chroma = taken.frame.format.full_chroma();
            let planes = match taken.handle {
                // A device slot: the planes are offsets into the handle.
                Some(_) => [
                    lowlat_plane {
                        data: core::ptr::null(),
                        pitch,
                        offset: 0,
                    },
                    lowlat_plane {
                        data: core::ptr::null(),
                        pitch,
                        offset: taken.frame.uv_offset as u64,
                    },
                    lowlat_plane {
                        data: core::ptr::null(),
                        pitch: if full_chroma { pitch } else { 0 },
                        offset: if full_chroma {
                            taken.frame.v_offset as u64
                        } else {
                            0
                        },
                    },
                ],
                None => [
                    lowlat_plane {
                        data: taken.y,
                        pitch,
                        offset: 0,
                    },
                    lowlat_plane {
                        data: taken.uv,
                        pitch,
                        offset: 0,
                    },
                    lowlat_plane {
                        data: taken.v,
                        pitch: if taken.v.is_null() { 0 } else { pitch },
                        offset: 0,
                    },
                ],
            };
            let filled = lowlat_frame {
                size,
                kind: if taken.handle.is_some() {
                    lowlat_frame_kind::LOWLAT_FRAME_HANDLE
                } else {
                    lowlat_frame_kind::LOWLAT_FRAME_PLANES
                } as u32,
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
                planes,
                slot: u32::try_from(taken.index).unwrap_or(u32::MAX),
                handle_kind: match taken.handle {
                    Some(_) => lowlat_handle_kind::LOWLAT_HANDLE_OPAQUE_FD,
                    None => lowlat_handle_kind::LOWLAT_HANDLE_NONE,
                } as u32,
                fd: taken.handle.map_or(-1, |h| h.fd),
                allocation: taken.handle.map_or(0, |h| h.allocation),
                handle_size: taken.handle.map_or(0, |h| h.size as u64),
                modifier: 0,
                full_range: taken.frame.full_range,
                arrived_us,
            };
            // As much as the caller's size reaches, and no more: a caller
            // built against an older header gets the fields it knows.
            // SAFETY: `frame` is at least `size` bytes by the caller's
            // contract; `filled` is a whole one, both plain data.
            core::ptr::copy_nonoverlapping(
                (&raw const filled).cast::<u8>(),
                frame.cast::<u8>(),
                (size as usize).min(core::mem::size_of::<lowlat_frame>()),
            );
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
/// version takes**: every picture was copied before it was handed out, on
/// the host or on the device, and is reusable once released.
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
                out.write(described(handle, &attempt, &received));
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
                    let event = described(handle, &attempt, &received);
                    let written = match event.kind {
                        LOWLAT_EVENT_USER_DATA => event.body.user_data.body_len,
                        LOWLAT_EVENT_GUEST_LIST => event.body.guest_list.body_len,
                        _ => 0,
                    };
                    body_len.write(written);
                    out.write(event);
                    LOWLAT_OK
                }
            }
        })
    }
}

/// The pointer's picture, decoded into the handle's own buffer: the pointer
/// the application is lent, and how long the picture is. A picture the
/// reader refuses is counted and not delivered.
fn decode_cursor(handle: &lowlat_client, png: &[u8]) -> (*const u8, u32) {
    if png.is_empty() {
        return (core::ptr::null(), 0);
    }
    let mut scratch = handle
        .cursor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match ::lowlat_client::cursor::decode_png(png, &mut scratch) {
        Ok(_) => (
            scratch.as_ptr(),
            u32::try_from(scratch.len()).unwrap_or(u32::MAX),
        ),
        Err(refusal) => {
            let telemetry = handle
                .held()
                .seam
                .telemetry()
                .cursor_refused
                .fetch_add(1, Ordering::Relaxed);
            if telemetry == 0 {
                lowlat_common::log_warn!(
                    "client: cursor picture refused, why={refusal:?} bytes={}",
                    png.len()
                );
            }
            (core::ptr::null(), 0)
        }
    }
}

/// Describe one event in the shape the boundary publishes.
fn described(
    handle: &lowlat_client,
    attempt: &str,
    received: &lowlat_common::events::Received<Event>,
) -> lowlat_event {
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
                Outcome::RelayUnreachable => (LOWLAT_OUTCOME_RELAY_UNREACHABLE, 0),
                Outcome::RelayRefused => (LOWLAT_OUTCOME_RELAY_REFUSED, 0),
                Outcome::RelayLost => (LOWLAT_OUTCOME_RELAY_LOST, 0),
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
        Event::Cursor {
            x,
            y,
            width,
            height,
            hot_x,
            hot_y,
            hidden,
            relative,
            suppressed,
            checksum,
            png,
        } => {
            let (image, image_len) = decode_cursor(handle, png);
            lowlat_event {
                kind: LOWLAT_EVENT_CURSOR,
                dropped,
                body: lowlat_event_body {
                    cursor: lowlat_cursor_event {
                        x: *x,
                        y: *y,
                        width: *width,
                        height: *height,
                        hot_x: *hot_x,
                        hot_y: *hot_y,
                        checksum: *checksum,
                        image,
                        image_len,
                        hidden: *hidden,
                        relative: *relative,
                        suppressed: *suppressed,
                        image_update: !image.is_null(),
                    },
                },
            }
        }
        Event::Rumble { pad, large, small } => lowlat_event {
            kind: LOWLAT_EVENT_RUMBLE,
            dropped,
            body: lowlat_event_body {
                rumble: lowlat_rumble_event {
                    pad: *pad,
                    large: *large,
                    small: *small,
                    reserved: [0; 2],
                },
            },
        },
        Event::GuestList { number, body } => lowlat_event {
            kind: LOWLAT_EVENT_GUEST_LIST,
            dropped,
            body: lowlat_event_body {
                guest_list: lowlat_guest_list_event {
                    number: *number,
                    body_len: u32::try_from(body.len()).unwrap_or(u32::MAX),
                },
            },
        },
        Event::PadReport {
            pad,
            kind,
            len,
            report,
        } => {
            let len = usize::from(*len).min(report.len());
            let mut lent = handle
                .pad_report
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *lent = *report;
            let kind = match kind {
                ::lowlat_core::pad::OutputKind::Output => {
                    lowlat_pad_report::LOWLAT_PAD_REPORT_OUTPUT
                }
                ::lowlat_core::pad::OutputKind::Feature => {
                    lowlat_pad_report::LOWLAT_PAD_REPORT_FEATURE
                }
            };
            lowlat_event {
                kind: LOWLAT_EVENT_PAD_REPORT,
                dropped,
                body: lowlat_event_body {
                    pad_report: lowlat_pad_report_event {
                        pad: *pad,
                        kind: kind as u32,
                        len: u32::try_from(len).unwrap_or(u32::MAX),
                        report: lent.as_ptr(),
                    },
                },
            }
        }
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
                offset: 0,
            }; 3],
            slot: 0,
            handle_kind: 0,
            fd: -1,
            allocation: 0,
            handle_size: 0,
            modifier: 0,
            full_range: false,
            arrived_us: 0,
        };
        assert_eq!(
            unsafe { lowlat_client_acquire_frame(handle, 0, 0, &raw mut frame) },
            LOWLAT_ERR_NOT_STARTED
        );
        // A caller built against minor 14 stamps the size the frame had then:
        // taken, where one byte less is not.
        frame.size = FRAME_MINOR_14 as u32;
        assert_eq!(
            unsafe { lowlat_client_acquire_frame(handle, 0, 0, &raw mut frame) },
            LOWLAT_ERR_NOT_STARTED,
            "a frame of minor 14's size was refused"
        );
        frame.size = FRAME_MINOR_14 as u32 - 1;
        assert_eq!(
            unsafe { lowlat_client_acquire_frame(handle, 0, 0, &raw mut frame) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        frame.size = core::mem::size_of::<lowlat_frame>() as u32;
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
            number: 0,
            cursor_images: 0,
            cursor_misses: 0,
            cursor_refused: 0,
            pad_reports_sent: 0,
            pad_reports_received: 0,
            pad_reports_dropped: 0,
            relay_address: [0; LOWLAT_ADDRESS_MAX],
            relay_port: 7,
            relayed: true,
            reserved: 0,
        };
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert_eq!(status.state, LOWLAT_CLIENT_IDLE);
        assert_eq!(taken(&status.relay_address), Some(""));
        assert_eq!((status.relay_port, status.relayed), (0, false));
        // A caller built against the header before the relay's fields gets
        // the fields it knows and nothing past them; one shorter still is
        // refused.
        status.size = u32::try_from(STATUS_MINOR_13).unwrap();
        status.relay_port = 7;
        status.state = 99;
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_OK
        );
        assert_eq!(status.state, LOWLAT_CLIENT_IDLE);
        assert_eq!(
            status.relay_port, 7,
            "a field past the caller's size was written"
        );
        status.size -= 1;
        assert_eq!(
            unsafe { lowlat_client_get_status(handle, &raw mut status) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        status.size = core::mem::size_of::<lowlat_client_status>() as u32;
        // The client's own figures: readable before a session, every one
        // zero, and a structure without its size refused.
        let mut metrics = lowlat_client_metrics {
            size: 0,
            connected_ms: 7,
            rtt_ms: 7,
            reserved: 0,
            control: lowlat_client_channel_metrics::default(),
            video: lowlat_client_channel_metrics::default(),
            audio: lowlat_client_channel_metrics::default(),
        };
        assert_eq!(
            unsafe { lowlat_client_get_metrics(handle, &raw mut metrics) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        metrics.size = core::mem::size_of::<lowlat_client_metrics>() as u32;
        assert_eq!(
            unsafe { lowlat_client_get_metrics(handle, &raw mut metrics) },
            LOWLAT_OK
        );
        assert_eq!(metrics.connected_ms, 0);
        assert_eq!(metrics.video.fragments, 0);
        assert_eq!(metrics.video.loss_30s.to_bits(), 0);
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
        // Without a decoder: the defaults ask for the first that opens, and
        // a machine with none refuses the creation before the setting is
        // ever read.
        let info = no_decoder();
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
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
                full_range: false,
            },
            raw_audio: false,
            legacy_cipher: true,
            shared_address_space: false,
            reserved: 0,
            server_count: 0,
            servers: [[0; LOWLAT_SERVER_MAX]; LOWLAT_SERVERS_MAX],
            relay: [0; LOWLAT_SERVER_MAX],
            relay_username: [0; LOWLAT_RELAY_CREDENTIAL_MAX],
            relay_password: [0; LOWLAT_RELAY_CREDENTIAL_MAX],
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

    /// The relay is read only when the caller's size reaches it, so a
    /// caller built against the header before it makes a direct attempt
    /// whatever lies past its structure; and a relay is refused without a
    /// credential or with a name that does not resolve, while the caller can
    /// still fix it.
    #[test]
    fn a_relay_is_read_only_where_the_size_reaches_and_needs_a_credential() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        let info = no_decoder();
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_OK
        );
        // SAFETY: plain data, for which zero is the default.
        let mut cfg: lowlat_client_config = unsafe { core::mem::zeroed() };
        let mut ours = lowlat_credentials {
            size: core::mem::size_of::<lowlat_credentials>() as u32,
            port: 0,
            reserved: 0,
            ufrag: [0; LOWLAT_ICE_MAX],
            pwd: [0; LOWLAT_ICE_MAX],
            fingerprint: [0; LOWLAT_FINGERPRINT_MAX],
            aes256: [0; LOWLAT_ICE_MAX],
        };
        let mut attempt = |cfg: &lowlat_client_config| {
            let status = unsafe {
                lowlat_client_new_attempt(
                    handle,
                    cfg,
                    c"relay".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            };
            unsafe { lowlat_client_end_connection(handle) };
            status
        };

        // An older caller: the relay's bytes are garbage it never set.
        put(&mut cfg.relay, "no-such-host.invalid:3478");
        cfg.size = u32::try_from(CONFIG_MINOR_13).unwrap();
        assert_eq!(
            attempt(&cfg),
            LOWLAT_OK,
            "a field past the caller's size was read"
        );
        cfg.size -= 1;
        assert_eq!(attempt(&cfg), LOWLAT_ERR_INVALID_ARGUMENT);

        cfg.size = core::mem::size_of::<lowlat_client_config>() as u32;
        assert_eq!(
            attempt(&cfg),
            LOWLAT_ERR_INVALID_ARGUMENT,
            "an unresolvable relay"
        );
        put(&mut cfg.relay, "203.0.113.1:3478");
        assert_eq!(
            attempt(&cfg),
            LOWLAT_ERR_INVALID_ARGUMENT,
            "a relay with no credential"
        );
        put(&mut cfg.relay_username, "user");
        assert_eq!(
            attempt(&cfg),
            LOWLAT_ERR_INVALID_ARGUMENT,
            "a relay with no password"
        );
        put(&mut cfg.relay_password, "password");
        assert_eq!(attempt(&cfg), LOWLAT_OK);
        unsafe { lowlat_client_destroy(handle) };
    }

    /// A frame kind the open decoder does not export is refused at
    /// creation, with the stage, before any device is opened.
    #[test]
    fn what_is_not_built_is_refused_at_creation() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        let mut info = no_decoder();
        info.decoder = lowlat_decoder::LOWLAT_DECODER_OPEN as u32;
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

    /// **The software decoder is trusted only on the library's own word.**
    /// Named by kind with a directory holding no pair, the answer is the
    /// runtime stage; named with no directory, whatever the machine has
    /// either opens as an LGPL build or is refused with the licence status,
    /// the runtime status or the profile status -- never quietly taken.
    #[test]
    fn the_software_decoder_opens_an_lgpl_pair_or_names_the_stage() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        // A directory with no pair in it: the runtime stage, never a walk --
        // unless the environment names a pair, which outranks the field.
        if std::env::var_os("LOWLAT_FFMPEG_DIR").is_none() {
            let mut info = no_decoder();
            info.decoder = lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32;
            let empty =
                std::env::temp_dir().join(format!("lowlat-abi-empty-{}", std::process::id()));
            std::fs::create_dir_all(&empty).expect("a temp dir");
            put(&mut info.device, &empty.display().to_string());
            assert_eq!(
                unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
                LOWLAT_ERR_NO_DECODER_RUNTIME
            );
            let _ = std::fs::remove_dir(&empty);
        }

        let mut info = no_decoder();
        info.decoder = lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32;
        let answer = unsafe { lowlat_client_create(&raw const info, &raw mut handle) };
        match answer {
            LOWLAT_OK => {
                let mut status: lowlat_client_status = unsafe { core::mem::zeroed() };
                status.size = core::mem::size_of::<lowlat_client_status>() as u32;
                assert_eq!(
                    unsafe { lowlat_client_get_status(handle, &raw mut status) },
                    LOWLAT_OK
                );
                assert_eq!(
                    status.backend,
                    lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32
                );
                unsafe { lowlat_client_destroy(handle) };
            }
            LOWLAT_ERR_NO_DECODER_LICENCE
            | LOWLAT_ERR_NO_DECODER_RUNTIME
            | LOWLAT_ERR_NO_DECODER_PROFILE => {
                assert!(handle.is_null());
            }
            other => panic!("the software decoder answered {other:?}"),
        }
        println!("software decoder: {answer:?}");
    }

    /// **Another decoder, chosen before a session.** A value that is not a
    /// decoder is refused as such, no decoder at all is refused as
    /// unsupported, a kind that does not open answers with its stage and
    /// leaves the choice as it was, and one that opens becomes the choice
    /// status reports. A session of the handle kind refuses the call.
    #[test]
    fn the_decoder_is_chosen_again_or_refused_with_the_choice_kept() {
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        let info = no_decoder();
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_OK
        );
        let backend = |handle| {
            let mut status: lowlat_client_status = unsafe { core::mem::zeroed() };
            status.size = core::mem::size_of::<lowlat_client_status>() as u32;
            assert_eq!(
                unsafe { lowlat_client_get_status(handle, &raw mut status) },
                LOWLAT_OK
            );
            status.backend
        };
        assert_eq!(backend(handle), lowlat_decoder::LOWLAT_DECODER_NONE as u32);
        assert_eq!(
            unsafe { lowlat_client_set_decoder(handle, 42, core::ptr::null()) },
            LOWLAT_ERR_INVALID_ARGUMENT
        );
        assert_eq!(
            unsafe {
                lowlat_client_set_decoder(
                    handle,
                    lowlat_decoder::LOWLAT_DECODER_NONE as u32,
                    core::ptr::null(),
                )
            },
            LOWLAT_ERR_DECODER_UNSUPPORTED
        );
        let empty = std::env::temp_dir().join(format!("lowlat-abi-switch-{}", std::process::id()));
        std::fs::create_dir_all(&empty).expect("a temp dir");
        let empty_c = std::ffi::CString::new(empty.display().to_string()).expect("a path");
        if std::env::var_os("LOWLAT_FFMPEG_DIR").is_none() {
            assert_eq!(
                unsafe {
                    lowlat_client_set_decoder(
                        handle,
                        lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32,
                        empty_c.as_ptr(),
                    )
                },
                LOWLAT_ERR_NO_DECODER_RUNTIME
            );
            assert_eq!(
                backend(handle),
                lowlat_decoder::LOWLAT_DECODER_NONE as u32,
                "a refused choice moved the decoder"
            );
        }
        let _ = std::fs::remove_dir(&empty);
        let software = unsafe {
            lowlat_client_set_decoder(
                handle,
                lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32,
                core::ptr::null(),
            )
        };
        match software {
            LOWLAT_OK => {
                assert_eq!(
                    backend(handle),
                    lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32
                );
            }
            LOWLAT_ERR_NO_DECODER_LICENCE
            | LOWLAT_ERR_NO_DECODER_RUNTIME
            | LOWLAT_ERR_NO_DECODER_PROFILE => {
                assert_eq!(backend(handle), lowlat_decoder::LOWLAT_DECODER_NONE as u32);
            }
            other => panic!("the software choice answered {other:?}"),
        }
        println!("software chosen: {software:?}");
        unsafe { lowlat_client_destroy(handle) };

        // A session of the handle kind, where this machine has the vendor's
        // decoder, refuses a move: its device slots are bound to the device.
        let mut info = no_decoder();
        info.decoder = lowlat_decoder::LOWLAT_DECODER_VENDOR as u32;
        info.frame_kind = lowlat_frame_kind::LOWLAT_FRAME_HANDLE as u32;
        if unsafe { lowlat_client_create(&raw const info, &raw mut handle) } == LOWLAT_OK {
            assert_eq!(
                unsafe {
                    lowlat_client_set_decoder(
                        handle,
                        lowlat_decoder::LOWLAT_DECODER_OPEN as u32,
                        core::ptr::null(),
                    )
                },
                LOWLAT_ERR_DECODER_UNSUPPORTED
            );
            assert_eq!(
                backend(handle),
                lowlat_decoder::LOWLAT_DECODER_VENDOR as u32
            );
            unsafe { lowlat_client_destroy(handle) };
        } else {
            println!("no vendor decoder here: the handle session's refusal not exercised");
        }
    }

    /// **The table runs from zero until false, and every slot answers.**
    /// A slot is the same thing on every call; an available row is one
    /// creation opens by its own values, an unavailable one decodes
    /// nothing and says why; a bad out-parameter is false rather than a
    /// write. On a machine with no decoder every slot is unavailable, which
    /// is an answer.
    #[test]
    fn the_decoders_enumerate_until_false() {
        assert!(!unsafe { lowlat_enum_decoders(0, core::ptr::null_mut()) });
        // SAFETY: plain data.
        let mut row: lowlat_decoder_info = unsafe { core::mem::zeroed() };
        row.size = 4;
        assert!(
            !unsafe { lowlat_enum_decoders(0, &raw mut row) },
            "a short size"
        );

        // A caller built against the row as it was before `driver`: the
        // fields it knows are filled, nothing past its size is touched.
        let older = DECODER_INFO_MINOR_12 as u32;
        let mut short: lowlat_decoder_info = unsafe { core::mem::zeroed() };
        short.size = older;
        short.driver[0] = 0x55;
        assert!(unsafe { lowlat_enum_decoders(0, &raw mut short) });
        assert_eq!(short.size, older, "the caller's size is the caller's");
        assert_eq!(short.driver[0], 0x55, "written past the caller's size");
        assert_ne!(short.name[0], 0, "the older fields were not filled");

        let slots = ::lowlat_client::enumerate::SLOTS;
        let mut count = 0;
        let mut available = 0;
        loop {
            row.size = core::mem::size_of::<lowlat_decoder_info>() as u32;
            if !unsafe { lowlat_enum_decoders(count, &raw mut row) } {
                break;
            }
            assert_eq!(row.index, count);
            assert_eq!(row.name[LOWLAT_DECODER_NAME_MAX - 1], 0, "terminated");
            assert_eq!(row.driver[LOWLAT_DECODER_NAME_MAX - 1], 0, "terminated");
            let name = unsafe { core::ffi::CStr::from_ptr(row.name.as_ptr()) };
            let driver = unsafe { core::ffi::CStr::from_ptr(row.driver.as_ptr()) };
            let device = unsafe { core::ffi::CStr::from_ptr(row.device.as_ptr()) };
            println!(
                "[{}] {} {:?} ({:?}) on {:?}: h264 {} hevc {} 10 {} 444 {} 444/10 {} handle {}",
                row.index,
                if row.available {
                    "available"
                } else {
                    "unavailable"
                },
                name,
                driver,
                device,
                row.h264,
                row.hevc,
                row.hevc_10,
                row.hevc_444,
                row.hevc_444_10,
                row.handle
            );
            let expected = if count < ::lowlat_client::enumerate::OPEN_SLOTS {
                lowlat_decoder::LOWLAT_DECODER_OPEN
            } else if count < slots - 1 {
                lowlat_decoder::LOWLAT_DECODER_VENDOR
            } else {
                lowlat_decoder::LOWLAT_DECODER_SOFTWARE
            } as u32;
            assert_eq!(row.decoder, expected, "slot {count}");
            let interface = match expected {
                x if x == lowlat_decoder::LOWLAT_DECODER_OPEN as u32 => "VA-API",
                x if x == lowlat_decoder::LOWLAT_DECODER_VENDOR as u32 => "NVDEC",
                _ => "libavcodec",
            };
            let label = name.to_str().expect("a label in ASCII");
            assert!(
                label == interface || label.starts_with(&format!("{interface} [")),
                "a label off the grammar: {label}"
            );
            assert!(
                !driver.to_bytes().is_empty(),
                "a slot without the driver's words or a reason"
            );
            if !row.available {
                assert!(
                    !(row.h264 || row.hevc || row.hevc_10 || row.hevc_444 || row.hevc_444_10),
                    "an unavailable slot with a capability"
                );
                assert!(!row.handle);
                count += 1;
                continue;
            }
            available += 1;
            assert!(
                row.h264 || row.hevc,
                "an available row that decodes nothing"
            );
            assert_eq!(
                row.handle,
                row.decoder == lowlat_decoder::LOWLAT_DECODER_VENDOR as u32
            );
            // Every available row opens by its own values, planes; the
            // software one refuses the handle kind.
            let mut info = no_decoder();
            info.decoder = row.decoder;
            info.device = row.device;
            let mut handle: *mut lowlat_client = core::ptr::null_mut();
            assert_eq!(
                unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
                LOWLAT_OK,
                "slot {count} did not open by its own values"
            );
            unsafe { lowlat_client_destroy(handle) };
            if row.decoder == lowlat_decoder::LOWLAT_DECODER_SOFTWARE as u32 {
                info.frame_kind = lowlat_frame_kind::LOWLAT_FRAME_HANDLE as u32;
                assert_eq!(
                    unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
                    LOWLAT_ERR_DECODER_UNSUPPORTED
                );
            }
            count += 1;
        }
        assert_eq!(count, slots, "the table's length");
        println!("{available} of {count} slots available");
    }

    /// The host's own admission, one guest and no stream: the peer the
    /// session tests here connect to on loopback.
    #[cfg(feature = "host")]
    fn a_host() -> ::lowlat_host::admission::Admission {
        use ::lowlat_host::admission::{self, Admission};
        Admission::new(admission::Config {
            microphone: None,
            pad_sink: None,
            exclusive_pointer: false,
            rumble_probe: false,
            exclusive_hold_ms: ::lowlat_host::floor::HOLD_MS,
            cg_level: 1,
            base_port: 0,
            shared_address_space: false,
            max_guests: 1,
            servers: Vec::new(),
            stream: None,
        })
    }

    /// Attempt `id` on the handle: the credentials for its offer.
    #[cfg(feature = "host")]
    fn an_offer(handle: *mut lowlat_client, id: &core::ffi::CStr) -> lowlat_credentials {
        let mut ours: lowlat_credentials = unsafe { core::mem::zeroed() };
        ours.size = core::mem::size_of::<lowlat_credentials>() as u32;
        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    core::ptr::null(),
                    id.as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_OK
        );
        ours
    }

    /// A session through the boundary against the host's own admission on
    /// loopback, the exchange relayed by hand as a signaling service would:
    /// the handle with its session up, and the host holding the other end.
    #[cfg(feature = "host")]
    fn a_session() -> (*mut lowlat_client, ::lowlat_host::admission::Admission) {
        let mut host = a_host();
        let mut handle: *mut lowlat_client = core::ptr::null_mut();
        let info = no_decoder();
        assert_eq!(
            unsafe { lowlat_client_create(&raw const info, &raw mut handle) },
            LOWLAT_OK
        );
        let ours = an_offer(handle, c"a");
        connect(handle, &mut host, c"a", &ours);
        (handle, host)
    }

    /// Attempt `id`, offered with `ours`, through to a session with `host`.
    #[cfg(feature = "host")]
    fn connect(
        handle: *mut lowlat_client,
        host: &mut ::lowlat_host::admission::Admission,
        id: &core::ffi::CStr,
        ours: &lowlat_credentials,
    ) {
        use ::lowlat_core::conn::Kind;
        use ::lowlat_host::admission::{self, Event as HostEvent};
        use std::time::Instant;

        let name = id.to_str().expect("an identifier in text");
        let text = |field: &[c_char]| taken(field).expect("a terminated field").to_string();
        host.new_attempt(
            name,
            admission::Peer {
                ufrag: text(&ours.ufrag),
                pwd: text(&ours.pwd),
                aes256: Some(text(&ours.aes256)),
                transport: admission::Transport::Bud,
                fingerprint: Some(text(&ours.fingerprint)),
                permissions: ::lowlat_host::inject::Permissions::default(),
                owner: false,
            },
        )
        .expect("the host registered the offer");
        let answer = host.begin_p2p(name, 0).expect("the host answered");
        let mut theirs: lowlat_credentials = unsafe { core::mem::zeroed() };
        theirs.size = core::mem::size_of::<lowlat_credentials>() as u32;
        put(&mut theirs.ufrag, &answer.ufrag);
        put(&mut theirs.pwd, &answer.pwd);
        put(&mut theirs.fingerprint, &answer.fingerprint);
        put(&mut theirs.aes256, &answer.aes256);
        assert_eq!(
            unsafe { lowlat_client_begin_p2p(handle, id.as_ptr(), &raw const theirs) },
            LOWLAT_OK
        );

        let candidate = |addr: std::net::SocketAddr, sync: bool, reflexive: bool, lan: bool| {
            let mut cand = lowlat_candidate {
                size: core::mem::size_of::<lowlat_candidate>() as u32,
                port: addr.port(),
                sync,
                reflexive,
                lan,
                address: [0; LOWLAT_ADDRESS_MAX],
            };
            put(&mut cand.address, &addr.ip().to_string());
            unsafe { lowlat_client_add_candidate(handle, id.as_ptr(), &raw const cand) };
        };
        let marker: std::net::SocketAddr = "1.2.3.4:1234".parse().expect("an address");
        let began = Instant::now();
        let (mut client_up, mut host_up) = (false, false);
        while began.elapsed() < Duration::from_secs(20) && !(client_up && host_up) {
            loop {
                let mut event = core::mem::MaybeUninit::<lowlat_event>::uninit();
                let polled = unsafe {
                    lowlat_client_poll_events(
                        handle,
                        0,
                        event.as_mut_ptr(),
                        core::ptr::null_mut(),
                        core::ptr::null_mut(),
                    )
                };
                if polled != LOWLAT_OK {
                    break;
                }
                let event = unsafe { event.assume_init() };
                match event.kind {
                    lowlat_event_type::LOWLAT_EVENT_CANDIDATE => {
                        let cand = unsafe { event.body.candidate };
                        let ip = taken(&cand.address)
                            .and_then(|text| text.parse().ok())
                            .expect("an address");
                        host.add_candidate(
                            name,
                            std::net::SocketAddr::new(ip, cand.port),
                            false,
                            Kind::marked(cand.lan, cand.from_stun),
                        );
                    }
                    lowlat_event_type::LOWLAT_EVENT_READY => {
                        host.add_candidate(name, marker, true, Kind::Direct);
                    }
                    lowlat_event_type::LOWLAT_EVENT_ESTABLISHED => client_up = true,
                    lowlat_event_type::LOWLAT_EVENT_ENDED => panic!("the client ended"),
                    _ => {}
                }
            }
            // What the host says of an earlier attempt, its departure read
            // late, is not this one's.
            while let Some(received) = host.poll_event() {
                match received.event {
                    HostEvent::Candidate {
                        attempt,
                        addr,
                        from_stun,
                        lan,
                        ..
                    } if attempt == name => candidate(addr, false, from_stun, lan),
                    HostEvent::Ready { attempt } if attempt == name => {
                        candidate(marker, true, false, false);
                    }
                    HostEvent::Established { attempt, .. } if attempt == name => host_up = true,
                    HostEvent::Ended { attempt, outcome } if attempt == name => {
                        panic!("the host ended: {outcome:?}")
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(client_up && host_up, "the pair did not establish");
    }

    /// **A call made while the session leaves is answered at once**, not
    /// after the departure: the departure is a grace for its message to cross
    /// and two joins, and none of it holds the handle. The status reads as no
    /// attempt, and a new attempt is refused as one already started until the
    /// departure is over, then taken. The departure is timed as well, because
    /// a call answered after it had ended would show nothing.
    #[cfg(feature = "host")]
    #[test]
    fn a_call_made_while_the_session_leaves_is_answered_at_once() {
        use ::lowlat_host::admission::{Event as HostEvent, Outcome as HostOutcome};
        use std::time::Instant;

        let (handle, mut host) = a_session();
        let address = handle as usize;
        let leaving = std::thread::spawn(move || {
            let began = Instant::now();
            unsafe { lowlat_client_end_connection(address as *mut lowlat_client) };
            began.elapsed()
        });
        // Well inside the departure's grace.
        std::thread::sleep(Duration::from_millis(50));
        let mut status: lowlat_client_status = unsafe { core::mem::zeroed() };
        status.size = core::mem::size_of::<lowlat_client_status>() as u32;
        status.state = 99;
        let began = Instant::now();
        let read = unsafe { lowlat_client_get_status(handle, &raw mut status) };
        let answered = began.elapsed();
        let mut ours: lowlat_credentials = unsafe { core::mem::zeroed() };
        ours.size = core::mem::size_of::<lowlat_credentials>() as u32;
        let during = unsafe {
            lowlat_client_new_attempt(
                handle,
                core::ptr::null(),
                c"b".as_ptr(),
                lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                &raw mut ours,
            )
        };
        let left = leaving.join().expect("the leaving thread");

        assert!(
            left >= Duration::from_millis(200),
            "the departure took {left:?}, so nothing was made during it"
        );
        assert_eq!(read, LOWLAT_OK);
        assert!(
            answered < Duration::from_millis(50),
            "the status waited {answered:?} behind the departure"
        );
        assert_eq!(status.state, LOWLAT_CLIENT_IDLE);
        assert_eq!(
            during, LOWLAT_ERR_ALREADY_STARTED,
            "a new attempt was taken while the last one was still leaving"
        );
        assert_eq!(
            unsafe {
                lowlat_client_new_attempt(
                    handle,
                    core::ptr::null(),
                    c"b".as_ptr(),
                    lowlat_transport::LOWLAT_TRANSPORT_BUD as u32,
                    &raw mut ours,
                )
            },
            LOWLAT_OK,
            "the departure over, a new attempt was refused"
        );

        // The departure itself still reaches the host.
        let began = Instant::now();
        let mut outcome = None;
        while began.elapsed() < Duration::from_secs(5) && outcome.is_none() {
            while let Some(received) = host.poll_event() {
                if let HostEvent::Ended { outcome: ended, .. } = received.event {
                    outcome = Some(ended);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(outcome, Some(HostOutcome::PeerLeft(0)));
        host.end_connection("a");
        unsafe { lowlat_client_destroy(handle) };
    }

    /// **A second session on the same handle waits for its pictures as the
    /// first did.** A departure closes the picture queue so no waiter is left
    /// stranded, and the next attempt takes waiters again from the moment it
    /// is made, before its answer as well as after. Without that, every
    /// acquire after a reconnect comes back at once and a render loop paced
    /// by the wait spins. There is no decoder, so no picture ever comes and
    /// every acquire here runs to its timeout.
    #[cfg(feature = "host")]
    #[test]
    fn a_second_session_on_the_handle_waits_for_its_pictures() {
        use std::time::Instant;

        let waited = |handle: *mut lowlat_client| {
            let mut frame: lowlat_frame = unsafe { core::mem::zeroed() };
            frame.size = core::mem::size_of::<lowlat_frame>() as u32;
            let began = Instant::now();
            let status = unsafe { lowlat_client_acquire_frame(handle, 0, 100, &raw mut frame) };
            (status, began.elapsed())
        };
        let (handle, mut host) = a_session();
        let (status, first) = waited(handle);
        assert_eq!(status, LOWLAT_TIMEOUT);
        assert!(
            first >= Duration::from_millis(80),
            "the first session's acquire came back after {first:?}, so there is nothing to compare"
        );
        unsafe { lowlat_client_end_connection(handle) };
        host.end_connection("a");

        let ours = an_offer(handle, c"b");
        let (status, offered) = waited(handle);
        assert_eq!(status, LOWLAT_TIMEOUT);
        assert!(
            offered >= Duration::from_millis(80),
            "an acquire after the next offer came back after {offered:?}"
        );
        connect(handle, &mut host, c"b", &ours);
        let (status, second) = waited(handle);
        assert_eq!(status, LOWLAT_TIMEOUT);
        assert!(
            second >= Duration::from_millis(80),
            "the second session's acquire came back after {second:?}"
        );

        unsafe { lowlat_client_end_connection(handle) };
        host.end_connection("b");
        unsafe { lowlat_client_destroy(handle) };
    }

    /// **A new attempt starts from nothing the last session left.** An event
    /// the application never took is not handed out afterwards under the new
    /// attempt's name, and the status and the figures describe the new
    /// attempt -- connecting rather than over, nothing counted -- until its
    /// own session says otherwise.
    #[cfg(feature = "host")]
    #[test]
    fn a_new_attempt_starts_from_nothing_the_last_session_left() {
        use std::time::Instant;

        let read = |handle: *mut lowlat_client| {
            let mut status: lowlat_client_status = unsafe { core::mem::zeroed() };
            status.size = core::mem::size_of::<lowlat_client_status>() as u32;
            let mut metrics: lowlat_client_metrics = unsafe { core::mem::zeroed() };
            metrics.size = core::mem::size_of::<lowlat_client_metrics>() as u32;
            assert_eq!(
                unsafe { lowlat_client_get_status(handle, &raw mut status) },
                LOWLAT_OK
            );
            assert_eq!(
                unsafe { lowlat_client_get_metrics(handle, &raw mut metrics) },
                LOWLAT_OK
            );
            (status, metrics)
        };
        let (handle, mut host) = a_session();
        // A message from the host that the application never takes: sent,
        // and waited for until the control channel has counted it.
        let (_, before) = read(handle);
        let guests = host.guests();
        assert!(host.send_user_data(guests[0].number, 5, b"left behind"));
        let began = Instant::now();
        let (status, metrics) = loop {
            let (status, metrics) = read(handle);
            if metrics.control.messages > before.control.messages
                || began.elapsed() > Duration::from_secs(5)
            {
                break (status, metrics);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(
            metrics.control.messages > before.control.messages,
            "the host's message never arrived"
        );
        assert_eq!(status.state, LOWLAT_CLIENT_ESTABLISHED);
        assert!(metrics.connected_ms > 0);
        unsafe { lowlat_client_end_connection(handle) };
        host.end_connection("a");

        an_offer(handle, c"b");
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
            "an event the last session left came out under the new attempt"
        );
        let (status, metrics) = read(handle);
        assert_eq!(
            status.state, LOWLAT_CLIENT_CONNECTING,
            "the new attempt read as the last session's state"
        );
        assert_eq!(
            metrics.connected_ms, 0,
            "the last session's time carried over"
        );
        assert_eq!(
            metrics.control.messages, 0,
            "the last session's messages were counted as the new attempt's"
        );

        unsafe { lowlat_client_end_connection(handle) };
        unsafe { lowlat_client_destroy(handle) };
    }
}
