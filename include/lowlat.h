/** @file
 * lowlat - the public C ABI.
 *
 * Generated from the Rust definitions. Do not edit; see crates/sdk/cbindgen.toml.
 */

#pragma once

#include <stdint.h>
#include <stdbool.h>

/// `noexcept` in C++ and nothing at all in C, so one header serves both.
///
/// **Every function here is nothrow by construction.** A panic is caught at
/// the boundary and comes back as a status, so a C++ caller emitting landing
/// pads around every call is paying for an exception that cannot arrive.
///
/// **MSVC is tested first**, because it reports `__cplusplus` as 199711L
/// unless it is asked not to, and any version of it that defines `_MSVC_LANG`
/// is a C++ compiler that has had `noexcept` for a decade.
#if defined(_MSVC_LANG) || (defined(__cplusplus) && __cplusplus >= 201103L)
#define LOWLAT_NOEXCEPT noexcept
#else
#define LOWLAT_NOEXCEPT
#endif

/// The two halves, declared unless the application says otherwise.
///
/// **A plain include declares everything.** An application built against a
/// library that carries one half defines `LOWLAT_NO_HOST` or
/// `LOWLAT_NO_CLIENT` before including this, and a call into the missing half
/// fails to compile rather than to link. `lowlat_features()` reports the same
/// two halves for the library that was actually loaded.
#if !defined(LOWLAT_NO_HOST) && !defined(LOWLAT_HOST)
#define LOWLAT_HOST
#endif
#if !defined(LOWLAT_NO_CLIENT) && !defined(LOWLAT_CLIENT)
#define LOWLAT_CLIENT
#endif

/// The major version, raised only when something already published changes.
#define LOWLAT_ABI_MAJOR 0

/// The minor version, raised when surface is appended.
#define LOWLAT_ABI_MINOR 8

/// The host half is in this build: every `lowlat_host_*` entry point exists.
#define LOWLAT_FEATURE_HOST 1

/// The client half is in this build: every `lowlat_client_*` entry point exists.
#define LOWLAT_FEATURE_CLIENT 2

#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))
/// How many reflexive servers a seam may be given, and how long each may be.
///
/// **A fixed array rather than a pointer and a count**, so the structure stays
/// one blittable block with nothing in it to free. Four is already more than
/// any host here has ever been configured with.
#define LOWLAT_SERVERS_MAX 4

/// The longest textual `host:port` for one of them.
#define LOWLAT_SERVER_MAX 64

/// The longest attempt identifier carried across this boundary.
///
/// **A fixed array rather than a pointer**, because nothing crosses here that
/// the application has to free (docs/06-api.md 10). An identifier longer than
/// this is the application's own, so it is truncated on the way out rather
/// than refused: the event still says what happened, and the application
/// already holds the identifier it made up.
#define LOWLAT_ATTEMPT_MAX 128

/// The longest textual address, which is what an address for a peer's
/// signaling to forward has to be anyway.
#define LOWLAT_ADDRESS_MAX 46

/// **Sized for the longest kind of identity, which is a device path.** These
/// are not display connector names, which are short: the same bound carries
/// the sound server's own name for a device, where a USB output's serial and
/// profile land it past a hundred characters, and a display identity on
/// Windows is an operating-system device path, which is bounded at 260. A name
/// that does not fit is truncated silently and then resolves to nothing, so
/// the bound is set by the worst case rather than by the observed one.
#define LOWLAT_OUTPUT_MAX 260

/// The longest credential this boundary carries.
///
/// **Sized by the largest of them, which is the media key.** It travels as
/// text and measures 254 characters, so anything shorter than this truncates a
/// key into something that decrypts nothing and reports no reason.
#define LOWLAT_ICE_MAX 256

/// The longest fingerprint.
#define LOWLAT_FINGERPRINT_MAX 112
#endif

#if defined(LOWLAT_HOST)
/// The most guests a host may advertise, which is what the ring memory per
/// guest is sized against.
#define LOWLAT_GUESTS_MAX 16

/// Every guest at once, where a guest number is taken.
///
/// **Zero, because guest numbers start at one.** A message aimed here reaches
/// everyone seated rather than nobody.
#define LOWLAT_GUEST_ALL 0

/// The most this host will encode sound at.
///
/// **A ceiling rather than a range**, because the codec silently clamps its own
/// and an application that asked for ten megabits would be told yes and given
/// something else. Well above any rate stereo desktop sound is worth.
#define LOWLAT_AUDIO_KBPS_MAX 512

/// Samples one microphone packet can carry.
///
/// **What a buffer must hold**, not what a packet usually is: a peer sends ten
/// milliseconds and this is twice that, because the length is the peer's to
/// write and a receiver sizes its own work.
#define LOWLAT_MICROPHONE_SAMPLES_MAX 960

/// Samples a second a microphone packet carries.
#define LOWLAT_MICROPHONE_SAMPLE_RATE 48000

/// How many channels it carries.
#define LOWLAT_MICROPHONE_CHANNELS 1
#endif

#if defined(LOWLAT_CLIENT)
/// The longest name a decoder's row carries.
#define LOWLAT_DECODER_NAME_MAX 128

/// The sound codec on the wire, as `lowlat_client_status.audio_codec`
/// reports it.
#define LOWLAT_AUDIO_OPUS 1

#define LOWLAT_AUDIO_PCM 2

/// Modifier bits for `lowlat_client_send_key`. The lock bits are the toggles'
/// state, which a host reads to keep its own locks in step.
#define LOWLAT_MOD_LSHIFT 1

#define LOWLAT_MOD_RSHIFT 2

#define LOWLAT_MOD_LCTRL 64

#define LOWLAT_MOD_RCTRL 128

#define LOWLAT_MOD_LALT 256

#define LOWLAT_MOD_RALT 512

#define LOWLAT_MOD_LGUI 1024

#define LOWLAT_MOD_RGUI 2048

#define LOWLAT_MOD_NUM 4096

#define LOWLAT_MOD_CAPS 8192

/// Mouse buttons for `lowlat_client_send_mouse_button`.
#define LOWLAT_MOUSE_LEFT 1

#define LOWLAT_MOUSE_MIDDLE 2

#define LOWLAT_MOUSE_RIGHT 3

#define LOWLAT_MOUSE_X1 4

#define LOWLAT_MOUSE_X2 5

/// Pad buttons for `lowlat_client_send_pad_button`, by index.
///
/// **Not the bits of `lowlat_pad_state`**: the two forms number the
/// buttons differently and neither is derivable from the other.
#define LOWLAT_PAD_A 0

#define LOWLAT_PAD_B 1

#define LOWLAT_PAD_X 2

#define LOWLAT_PAD_Y 3

#define LOWLAT_PAD_BACK 4

#define LOWLAT_PAD_GUIDE 5

#define LOWLAT_PAD_START 6

#define LOWLAT_PAD_LSTICK 7

#define LOWLAT_PAD_RSTICK 8

#define LOWLAT_PAD_LSHOULDER 9

#define LOWLAT_PAD_RSHOULDER 10

#define LOWLAT_PAD_DPAD_UP 11

#define LOWLAT_PAD_DPAD_DOWN 12

#define LOWLAT_PAD_DPAD_LEFT 13

#define LOWLAT_PAD_DPAD_RIGHT 14

/// Pad axes for `lowlat_client_send_pad_axis`. Sticks span the signed range;
/// triggers run from zero.
#define LOWLAT_PAD_AXIS_LX 0

#define LOWLAT_PAD_AXIS_LY 1

#define LOWLAT_PAD_AXIS_RX 2

#define LOWLAT_PAD_AXIS_RY 3

#define LOWLAT_PAD_AXIS_LT 4

#define LOWLAT_PAD_AXIS_RT 5

/// Button bits for `lowlat_pad_state`.
#define LOWLAT_PAD_STATE_DPAD_UP 1

#define LOWLAT_PAD_STATE_DPAD_DOWN 2

#define LOWLAT_PAD_STATE_DPAD_LEFT 4

#define LOWLAT_PAD_STATE_DPAD_RIGHT 8

#define LOWLAT_PAD_STATE_START 16

#define LOWLAT_PAD_STATE_BACK 32

#define LOWLAT_PAD_STATE_LSTICK 64

#define LOWLAT_PAD_STATE_RSTICK 128

#define LOWLAT_PAD_STATE_LSHOULDER 256

#define LOWLAT_PAD_STATE_RSHOULDER 512

#define LOWLAT_PAD_STATE_GUIDE 1024

#define LOWLAT_PAD_STATE_TOUCHPAD 2048

#define LOWLAT_PAD_STATE_A 4096

#define LOWLAT_PAD_STATE_B 8192

#define LOWLAT_PAD_STATE_X 16384

#define LOWLAT_PAD_STATE_Y 32768

/// No decoder has been built yet: no parameter set has arrived.
#define LOWLAT_DECODER_NONE_YET 0

/// A decoder exists and is being fed.
#define LOWLAT_DECODER_BUILT 1

/// No decoder can serve the stream; the ended event said so.
#define LOWLAT_DECODER_FAILED 2

/// Eight bits: a luma plane and an interleaved chroma plane at half the
/// rows.
#define LOWLAT_FORMAT_NV12 1

/// Ten bits in sixteen-bit samples, the value in the high bits; the same
/// two planes.
#define LOWLAT_FORMAT_P010 2

/// Eight bits, full chroma: three planes of the picture's size, luma then
/// the two chroma planes.
#define LOWLAT_FORMAT_YUV444 3

/// Ten bits in sixteen-bit samples, the value in the high bits; the same
/// three planes.
#define LOWLAT_FORMAT_YUV444_16 4

/// No attempt has been made.
#define LOWLAT_CLIENT_IDLE 0

/// Connectivity is running.
#define LOWLAT_CLIENT_CONNECTING 1

/// The session is up.
#define LOWLAT_CLIENT_ESTABLISHED 2

/// The session is over; the ended event says why.
#define LOWLAT_CLIENT_OVER 3
#endif

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
///   -500 to -599   decode
/// ```
///
/// A value is assigned once and never reused, including for a condition that
/// is removed.
typedef enum lowlat_status {
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
    /// The application already holds as many pictures as it may; one has to
    /// be released before another is acquired.
    LOWLAT_ERR_TOO_MANY_HELD = -7,
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
    /// A browser's offer carried no certificate digest, or one that is not
    /// a SHA-256 digest: nothing its handshake could be checked against.
    LOWLAT_ERR_FINGERPRINT = -107,
    /// Nothing is lit. There is no display to capture: a headless machine, or
    /// one whose session has not started.
    LOWLAT_ERR_NO_DISPLAY = -200,
    /// A display is lit and its framebuffer cannot be reached, which is what
    /// this process is allowed to do rather than what the machine has.
    LOWLAT_ERR_DISPLAY_UNREACHABLE = -201,
    /// The decoder's runtime library is not on the machine.
    LOWLAT_ERR_NO_DECODER_RUNTIME = -500,
    /// No render node opened for the decoder: none named opens, or none at
    /// all does.
    LOWLAT_ERR_NO_DECODER_DEVICE = -501,
    /// The device opened and decodes none of the profiles a stream could use.
    LOWLAT_ERR_NO_DECODER_PROFILE = -502,
    /// The decoder or the frame kind asked for is not in this build.
    LOWLAT_ERR_DECODER_UNSUPPORTED = -503,
} lowlat_status;

#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))
/// Which member of an event is the valid one.
typedef enum lowlat_event_type {
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
} lowlat_event_type;

/// Why an attempt finished.
typedef enum lowlat_outcome {
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
} lowlat_outcome;

/// The longest output identity carried across this boundary.
///
/// Which codec a stream is coded with: what a host is asked to encode, and
/// what a client reports its decoder was built for.
///
/// **Named by an enumeration and carried as an integer**, for the reason
/// `lowlat_status` is: the application writes this field, so the value
/// arriving is whatever it wrote.
typedef enum lowlat_codec {
    LOWLAT_CODEC_H264 = 1,
    LOWLAT_CODEC_HEVC = 2,
} lowlat_codec;
#endif

#if defined(LOWLAT_HOST)
/// How much colour the stream carries, relative to its luma.
///
/// **Named by an enumeration and carried as an integer**, the same way
/// `lowlat_codec` is, and an axis rather than a flag because it has
/// somewhere to go: a third layout is in wide use elsewhere even though
/// nothing here produces one.
typedef enum lowlat_chroma {
    /// Colour at half resolution in both directions, which is what a session
    /// runs at until a guest asks otherwise.
    LOWLAT_CHROMA_420 = 1,
    /// Colour at full resolution. Reported once a guest has asked for it and
    /// the offer has been granted, which needs the second codec and every
    /// encoder this host could select to be able to code it.
    LOWLAT_CHROMA_444 = 2,
} lowlat_chroma;

/// Which encoder to build.
typedef enum lowlat_encoder {
    /// **The default, and the right one.** A conversion target is allocated on
    /// the device the display is on and an encoder belonging to another cannot
    /// take it, so the encoder is a consequence of where the display is rather
    /// than a preference. Choosing one is for forcing a particular encoder on a
    /// machine where either would do.
    LOWLAT_ENCODER_FOLLOW_DISPLAY = 0,
    LOWLAT_ENCODER_OPEN = 1,
    LOWLAT_ENCODER_VENDOR = 2,
} lowlat_encoder;
#endif

#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))
/// How a picture is oriented.
///
/// **The coded picture never rotates.** A host sends the display's
/// orientation with its stream, and the peer presents the picture turned and
/// maps pointer coordinates against it; a client hands the same word out with
/// every picture.
typedef enum lowlat_rotation {
    LOWLAT_ROTATION_NONE = 1,
    LOWLAT_ROTATION_90 = 2,
    LOWLAT_ROTATION_180 = 3,
    LOWLAT_ROTATION_270 = 4,
} lowlat_rotation;
#endif

#if defined(LOWLAT_HOST)
/// Which congestion control level a session runs at.
///
/// **Zero is the most aggressive, not "off".** Its thresholds are all zero, so
/// every outstanding fragment classifies stale and congestion is declared on
/// every pass once the send window passes its floor. Sensitive is the default
/// and the one to leave alone.
///
/// **Adaptive is not a fourth tolerance.** The first three are tunings of one
/// detector and describe the whole of what a host does about congestion.
/// Adaptive runs the sensitive tuning and adds host-local signals that see
/// what the window floor hides. **Nothing is behind it yet**, so it behaves
/// exactly as sensitive today; it is named here so that a signal which earns
/// its measurement becomes a setting rather than a rebuild.
typedef enum lowlat_cg_level {
    LOWLAT_CG_LEVEL_AGGRESSIVE = 0,
    LOWLAT_CG_LEVEL_SENSITIVE = 1,
    LOWLAT_CG_LEVEL_RELAXED = 2,
    LOWLAT_CG_LEVEL_ADAPTIVE = 3,
} lowlat_cg_level;
#endif

/// How severe a log line is.
typedef enum lowlat_log_level {
    LOWLAT_LOG_ERROR = 0,
    LOWLAT_LOG_WARN = 1,
    LOWLAT_LOG_INFO = 2,
    LOWLAT_LOG_DEBUG = 3,
    LOWLAT_LOG_TRACE = 4,
} lowlat_log_level;

#if defined(LOWLAT_HOST)
/// Where a host sits between delay and picture.
///
/// **The only encoder tuning this boundary exposes.** An encoder has a dozen
/// knobs and almost none of them are an application's business; what an
/// application wants to say is whether its guests would rather wait less or
/// look at more. See [05 §4.1](../docs/05-host.md) for the levers this
/// moves and [06 §quality](../docs/06-api.md).
///
/// **Zero is the low-latency end, and that is deliberate**: a zeroed structure
/// has to mean the sensible default, and for a product whose first goal is
/// delay the sensible default is a bounded frame.
typedef enum lowlat_quality {
    LOWLAT_QUALITY_LOWEST_LATENCY = 0,
    LOWLAT_QUALITY_BALANCED = 1,
    LOWLAT_QUALITY_HIGHEST = 2,
} lowlat_quality;
#endif

#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))
/// Which pipe an attempt speaks.
typedef enum lowlat_transport {
    /// Authenticated records on the attempt socket. The default.
    LOWLAT_TRANSPORT_BUD = 0,
    /// A browser's data channel on the same socket.
    LOWLAT_TRANSPORT_WEB = 1,
} lowlat_transport;
#endif

#if defined(LOWLAT_CLIENT)
/// Which decoder a client is built on.
///
/// **The choice is by index, as the host's encoder is; unset, the first
/// that opens on the device named.** A machine without any is refused at
/// creation with the stage named, exactly as a host without an encoder is.
typedef enum lowlat_decoder {
    LOWLAT_DECODER_AUTO = 0,
    LOWLAT_DECODER_OPEN = 1,
    LOWLAT_DECODER_VENDOR = 2,
    /// No decoder: the session carries control and sound, and every picture
    /// is taken off the wire and dropped. A client with nowhere to draw.
    LOWLAT_DECODER_NONE = 3,
} lowlat_decoder;

/// How pictures leave the library.
typedef enum lowlat_frame_kind {
    /// Planes in memory the library owns for the lease.
    LOWLAT_FRAME_PLANES = 0,
    /// A device-level handle the application imports into its own device:
    /// the picture's planes at offsets into it. Only the vendor decoder
    /// exports one, so asking for it settles the decoder on the vendor's
    /// (`LOWLAT_DECODER_AUTO` then means the vendor's on any device), and
    /// the open decoder refuses it at creation with
    /// `LOWLAT_ERR_DECODER_UNSUPPORTED`.
    LOWLAT_FRAME_HANDLE = 1,
} lowlat_frame_kind;

/// What a frame of the handle kind carries.
typedef enum lowlat_handle_kind {
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
} lowlat_handle_kind;

typedef enum lowlat_fence_kind {
    /// Reusable now.
    LOWLAT_FENCE_NONE = 0,
} lowlat_fence_kind;

/// One client, as the application holds it.
///
/// Opaque: the application holds a pointer it cannot look inside, so what is
/// in here changes freely.
typedef struct lowlat_client lowlat_client;
#endif

#if defined(LOWLAT_HOST)
/// One host session, as the application holds it.
///
/// Opaque: the application holds a pointer it cannot look inside, so what is
/// in here changes freely.
typedef struct lowlat_host lowlat_host;
#endif

/// Where log lines go.
///
/// **The one place this library calls into an application**, and the single
/// exception to being poll-based. It is cold, it fires on whichever thread
/// logged, and it must not call back in.
typedef void (*lowlat_log_fn)(uint32_t level, const char *message, void *opaque);

#if defined(LOWLAT_HOST)
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
/// ([impl-plan](../docs/impl-plan.md), *Output selection*).
typedef struct lowlat_host_video_config {
    /// Set by the caller to `sizeof(lowlat_host_video_config)`.
    uint32_t size;
    /// **A ceiling, not a target.** Capture runs at the display's own rate and
    /// this is the most that is encoded from it. **Default: 60.**
    uint32_t fps;
    /// What the operator asked for, before it is divided among guests.
    /// **Default: 10.0.**
    double bitrate_mbps;
    /// The floor congestion control may not descend below. Lowered with the
    /// ceiling when it would otherwise sit above it. **Default: 1.0.**
    double min_bitrate_mbps;
    /// Emit at `fps` even when the picture has not changed.
    ///
    /// **A permission, not an instruction, and off by default.** There is no
    /// damage signal here, so nothing yet skips a repeated picture; a host that
    /// keeps sending costs bitrate rather than being wrong. Setting it promises
    /// to spend that bitrate whatever else becomes possible later.
    bool full_fps;
    uint8_t reserved[3];
    /// Which output to capture, by an identity from the enumeration. **Empty
    /// means whichever this host would pick on its own**, which is the output
    /// at the desktop's corner and then whatever is lit.
    char output[LOWLAT_OUTPUT_MAX];
} lowlat_host_video_config;

/// How sound is configured.
///
/// **Every field here is live.** Sound has no half that must be settled when
/// hosting starts: the device and the mute cost a reconnect the loop performs,
/// and the rest are read on the frame that uses them. So this is both what a
/// host starts with and what `lowlat_host_set_audio_config` takes.
typedef struct lowlat_host_audio_config {
    /// Set by the caller to `sizeof(lowlat_host_audio_config)`.
    uint32_t size;
    /// What the compressed form is encoded at, in kilobits a second.
    /// **Default: `lowlat_audio::encode::DEFAULT_BITRATE_KBPS`.**
    uint32_t bitrate_kbps;
    /// Whether sound is captured at all. Off gives the device back and puts
    /// the speakers at the desk back with it. **Default: on** -- a host that
    /// streams a desktop streams its sound.
    bool enabled;
    /// Whether a guest that asked for the uncompressed form may have it.
    ///
    /// **A permission, not a request**, and off by default: it costs an order
    /// of magnitude more of the uplink than the compressed form, which comes
    /// out of what is left for the picture.
    bool allow_uncompressed;
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
    bool mute_local;
    /// Whether a guest's microphone is taken.
    ///
    /// **Off by default, and it is the switch a guest waits on**: a peer sends
    /// no microphone audio until this host says it will take it, so nothing
    /// arrives while this is clear however the guest has configured itself.
    ///
    /// It costs a packet every ten milliseconds on the channel that carries
    /// control messages, which is why it is a decision rather than something
    /// switched on by polling for it.
    bool accept_microphone;
    uint8_t reserved[1];
    /// Which device to capture, by an identity from the enumeration. **Empty
    /// means the default output's monitor**, followed as the default changes.
    char device[LOWLAT_OUTPUT_MAX];
} lowlat_host_audio_config;

/// How a host is configured.
///
/// **There is no resolution here.** The display decides the picture's size, the
/// encoder follows it, and the application is told what it got rather than
/// asking for it; `fps` is a cap over whatever the display runs at, not a
/// target. A host that creates its own display chooses that display's size when
/// it creates it, which is a different question and not this field's.
typedef struct lowlat_host_config {
    /// Set by the caller to `sizeof(lowlat_host_config)`.
    uint32_t size;
    /// The base a guest's port bind walks from. **Default: 9000.**
    uint16_t base_port;
    uint16_t reserved;
    /// Advertised capacity. Above `LOWLAT_GUESTS_MAX` is refused rather than
    /// quietly reduced. **Default: 4.**
    uint32_t max_guests;
    /// One of `lowlat_codec`. Settled when hosting starts: one encode serves
    /// every seat and a session has one video configuration. **Default: H.264**,
    /// the one every peer decodes.
    uint32_t codec;
    /// One of `lowlat_encoder`. **Default: follow display.**
    uint32_t encoder;
    /// One of `lowlat_cg_level`. **Default: sensitive**, which is the tuning
    /// every other one is judged against. **Zero is not the default**, and a
    /// structure zeroed by its caller asks for the most aggressive setting
    /// rather than this one -- start from `lowlat_host_config_default`.
    uint32_t cg_level;
    /// One of `lowlat_quality`. **Settled when hosting starts**: it is what
    /// the encoder is built with, and one encode serves every seat.
    ///
    /// **What a host reports back is what it asked for, not what a device
    /// did.** No interface here says whether a driver honoured a quantiser
    /// floor or an effort level, and one measured takes the floor on one codec
    /// and ignores it on the other, so a host logs its request once per stream
    /// and does not claim more than that. **Default: lowest latency.**
    uint32_t quality;
    /// How long a guest keeps the pointer after its last movement, when
    /// `exclusive_pointer` is set. Clamped rather than refused: this is a
    /// comfort setting and the nearest usable value beats refusing to start.
    /// **Default: `::lowlat_host::floor::HOLD_MS`**, the figure the arbitration was
    /// tuned to.
    uint32_t exclusive_hold_ms;
    /// Whether one guest at a time may drive the pointer. Off means everybody
    /// drives it, which is a configuration rather than a fault. **Default: off.**
    bool exclusive_pointer;
    uint8_t reserved2[3];
    /// How many of `servers` are set. **Default: 0**, so a host consults
    /// nothing for its own address until an application names a server.
    uint32_t server_count;
    /// Reflexive servers, consulted for this host's own mapped address, each
    /// `host:port`.
    char servers[LOWLAT_SERVERS_MAX][LOWLAT_SERVER_MAX];
    /// The half of this that can also be set while the host runs.
    lowlat_host_video_config video;
    /// Sound, every field of which can also be set while the host runs.
    lowlat_host_audio_config audio;
} lowlat_host_config;

/// What a handle is created with.
///
/// **The caller sets `size`.** It is read rather than assumed, so this can grow
/// without breaking an application compiled against an older header
/// (docs/06-api.md 1).
typedef struct lowlat_host_create_info {
    uint32_t size;
} lowlat_host_create_info;

/// What a guest may drive.
typedef struct lowlat_permissions {
    bool keyboard;
    bool pointer;
    bool gamepad;
    uint8_t reserved;
} lowlat_permissions;

/// What signaling learned about a peer, handed over to register an attempt.
///
/// **Signaling is the application's**, so everything here arrived over a
/// transport this library does not have and does not want
/// ([04 §1](../docs/04-signaling.md)).
typedef struct lowlat_attempt_info {
    /// Set by the caller to `sizeof(lowlat_attempt_info)`.
    uint32_t size;
    uint32_t reserved;
    /// The application's own identifier for this attempt. Everything else in
    /// the seam is addressed by it.
    char attempt_id[LOWLAT_ATTEMPT_MAX];
    char ufrag[LOWLAT_ICE_MAX];
    char pwd[LOWLAT_ICE_MAX];
    /// The peer's media key material, as text.
    ///
    /// **Empty selects the legacy path**, which is a decision rather than a
    /// degradation: the offer either carried one or it did not, and which
    /// crypto a session uses follows from that ([00 §D2](../docs/00-overview.md)).
    char aes256[LOWLAT_ICE_MAX];
    /// What signaling says this peer may drive.
    lowlat_permissions permissions;
    /// Whether this peer owns the machine, which decides exactly one thing: it
    /// takes the pointer from another guest rather than waiting for it.
    bool owner;
    uint8_t reserved2[3];
    /// Which pipe the offer asked for, a `lowlat_transport` value.
    ///
    /// **Appended in minor 2.** An application built against minor 1 sets a
    /// smaller `size`, and everything from here on then reads as the native
    /// transport with no digest.
    uint32_t transport;
    /// The peer's certificate digest as its offer carried it, with or
    /// without the hash name. Read on the browser pipe and required there;
    /// ignored on the native one.
    char fingerprint[LOWLAT_FINGERPRINT_MAX];
} lowlat_attempt_info;
#endif

#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))
/// One address a peer might be reachable at.
typedef struct lowlat_candidate {
    /// Set by the caller to `sizeof(lowlat_candidate)`.
    uint32_t size;
    uint16_t port;
    /// **A readiness marker rather than an address**, and whatever address
    /// rides along with it is ignored. A peer may withhold every real
    /// candidate until it has seen one, so an application that never forwards
    /// one negotiates against a peer that never offers anything to check.
    bool sync;
    /// Whether a reflexive server reported this address to the peer. The
    /// path-opening probe goes only toward such a candidate; an address the
    /// peer knows directly needs no path opened ahead of its first check.
    /// Zero is safe when the application cannot say -- the punch still runs,
    /// without the early probe.
    bool reflexive;
    /// The exchange's lan marking, copied verbatim from the peer's
    /// signaling: directly routable, checked without ceremony. When both
    /// this and `reflexive` are set, lan wins. Neither set is a real class
    /// too -- a translated-path guess no server verified -- so zero for both
    /// is safe and means exactly that.
    bool lan;
    char address[LOWLAT_ADDRESS_MAX];
} lowlat_candidate;

/// One side's credentials for an attempt: what a host answers an offer with,
/// and what a client puts in its offer.
///
/// **A host's are generated at approval, not at registration.** They are
/// bound to the socket that was just opened for this attempt, so producing
/// them earlier binds them to nothing. A client's are minted with the attempt,
/// before it has a socket, so its `port` is zero.
typedef struct lowlat_credentials {
    /// Set by the caller to `sizeof(lowlat_credentials)`.
    uint32_t size;
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
    uint16_t port;
    uint16_t reserved;
    char ufrag[LOWLAT_ICE_MAX];
    char pwd[LOWLAT_ICE_MAX];
    char fingerprint[LOWLAT_FINGERPRINT_MAX];
    char aes256[LOWLAT_ICE_MAX];
} lowlat_credentials;
#endif

#if defined(LOWLAT_HOST)
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
typedef struct lowlat_guest {
    /// What this guest is addressed by, and what it finds itself by in a
    /// roster the application sends.
    uint32_t number;
    lowlat_permissions permissions;
    /// Whether this guest owns the machine, which decides exactly one thing:
    /// it takes the pointer from another guest rather than waiting for it.
    bool owner;
    uint8_t reserved[3];
    /// The identifier this attempt was registered under.
    ///
    /// **The link between the seam's two halves.** Everything before a guest is
    /// seated is addressed by attempt and everything after is addressed by
    /// number; without this, an application holding one peer per attempt cannot
    /// tell which peer an event about guest three concerns.
    char attempt[LOWLAT_ATTEMPT_MAX];
} lowlat_guest;

/// One sound output a host could capture.
typedef struct lowlat_audio_output {
    /// What to put in `lowlat_host_audio_config::device`.
    ///
    /// **The monitor of the output, not the output**, because that is the
    /// device a host reads: it carries what the speakers are playing.
    char id[LOWLAT_OUTPUT_MAX];
    /// What a person calls it, which is the name to show them.
    char name[LOWLAT_OUTPUT_MAX];
} lowlat_audio_output;

/// One output this host could be asked to capture.
typedef struct lowlat_output {
    /// What to ask for, and what a capture-changed event reports. **Stable
    /// across a mode change**, which is why it is not the size.
    char id[LOWLAT_OUTPUT_MAX];
    /// The connector's own name, which is what the session knows it by and
    /// what a person recognises.
    char name[LOWLAT_OUTPUT_MAX];
    uint32_t width;
    uint32_t height;
    /// Where it sits in the desktop around it, which is the space absolute
    /// input is expressed against. Zero when no session said, which is also
    /// the corner: with one output the two are the same answer.
    uint32_t x;
    uint32_t y;
} lowlat_output;

/// What a host is doing right now.
///
/// **What is happening, not what was asked for.** The picture's size is the
/// display's answer and the guest count is the room's; the settings that
/// produced them are read back through `lowlat_host_get_video_config`.
typedef struct lowlat_host_status {
    /// Set by the caller to `sizeof(lowlat_host_status)`.
    uint32_t size;
    /// Guests that are connected and addressable.
    uint32_t guests;
    /// The picture the stream is producing. **Zero before a display has been
    /// opened**, which is the honest answer: until then the size is the
    /// display's to decide and nothing here knows it.
    uint32_t width;
    uint32_t height;
    /// Whether this handle is hosting.
    bool running;
    /// Whether a sound device is being read right now.
    ///
    /// **Not what sound is set to.** Nothing is read while nobody is
    /// listening, so this is clear in an empty room however sound is
    /// configured; and it is also clear when the device could not be opened or
    /// has gone away, which is the case an application cannot learn any other
    /// way -- the settings still say enabled, because they are what was asked
    /// for.
    bool audio_active;
    /// Whether the stream codes ten bits a sample. **A flag rather than a
    /// number, because the axis has nowhere to go**: no encoder on this
    /// platform offers a depth above ten and one of them cannot describe one.
    bool ten_bit;
    uint8_t reserved[1];
    /// One of `lowlat_codec`, and **zero until something is being coded**.
    ///
    /// **What is coming out, not what was asked for.** A seated guest can move
    /// the codec and the depth while the stream runs, so the configuration
    /// stops being the answer as soon as one does.
    uint32_t codec;
    /// One of `lowlat_chroma`, and zero until something is being coded.
    uint32_t chroma;
    /// The sound device being read, empty when none is.
    ///
    /// **What it landed on, not what was asked for.** An empty request means
    /// the default output's monitor, and the sound server can move a stream
    /// while it runs, so this is the only place the two can be compared.
    char audio_device[LOWLAT_OUTPUT_MAX];
} lowlat_host_status;

/// What one of a guest's channels is doing.
///
/// **Named, not numbered.** A number here would be a stream index, and this
/// host produces one stream and switches which display feeds it, so there is
/// nothing to index. What genuinely differs between these figures is the
/// channel, and there are three of them.
typedef struct lowlat_channel_metrics {
    /// Distinct fragments put on the wire.
    ///
    /// **First transmissions only**, so the resend counters below divide into
    /// this as a loss rate rather than as a ratio of two overlapping counts.
    /// Pinned at the ceiling rather than wrapped, because a wrap reads as a
    /// session that has just started.
    uint32_t packets_sent;
    /// Retransmissions the peer asked for, and retransmissions the timeout had
    /// to find. **The two apart are the loss picture**: a path that reports its
    /// losses and one that swallows them need different answers.
    uint32_t fast_rts;
    uint32_t slow_rts;
    float bitrate_mbps;
    /// What this channel's payload cost this host to produce. **Zero on
    /// control**, which encodes nothing.
    float encode_ms;
    /// What the peer says this channel costs it to decode.
    ///
    /// **The peer's own figure.** It is the one number here this host cannot
    /// measure, and it arrives only because a guest volunteers it: zero until
    /// one has, and zero always on control.
    float decode_ms;
} lowlat_channel_metrics;

/// What one guest is doing.
///
/// **Its own structure behind its own call, and that is deliberate.** A guest
/// is delivered as an array element and an array element cannot carry a `size`
/// -- the caller walks it by stride -- so `lowlat_guest` is fixed for the
/// major version. These are the numbers most likely to grow, so they live
/// where growing them is free.
///
/// **Shared figures once, per-channel figures per channel.** The round trip,
/// the input stamps and the congestion count describe the guest and are here;
/// the counters and the rates describe one channel and are in each of the
/// three.
typedef struct lowlat_metrics {
    /// Set by the caller to `sizeof(lowlat_metrics)`.
    uint32_t size;
    /// How long this guest has been connected.
    uint32_t connected_ms;
    /// When each kind of input last arrived, on the same clock as
    /// `connected_ms`. **Zero means never**, which is not zero milliseconds
    /// ago -- an application kicking idle guests has to tell the two apart.
    uint32_t keyboard_ms;
    uint32_t pointer_ms;
    uint32_t gamepad_ms;
    /// Video frames sent to this guest.
    uint32_t frames;
    /// Fragments outstanding, and how many are past due. **These are the
    /// controller's own inputs**, so an application reads what the host is
    /// steering by rather than a second set derived elsewhere; together they
    /// are what "chronically behind" means.
    uint32_t window;
    uint32_t stale;
    /// Times congestion cost this guest rate.
    ///
    /// **One count, not one per channel.** Video is the only channel a rate
    /// controller steers, here and in every peer this talks to.
    uint32_t cg_events;
    /// The smoothed round trip to this peer. **One path, one figure**, which
    /// is why it is here rather than repeated in each channel.
    float network_ms;
    lowlat_channel_metrics control;
    lowlat_channel_metrics audio;
    lowlat_channel_metrics video;
} lowlat_metrics;
#endif

#if (defined(LOWLAT_HOST) || defined(LOWLAT_CLIENT))
/// A local candidate for the application to forward.
typedef struct lowlat_candidate_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
    char address[LOWLAT_ADDRESS_MAX];
    uint16_t port;
    /// Whether a reflexive server reported this one.
    bool from_stun;
    /// Whether the exchange should mark it lan: host candidates, and every
    /// IPv6 address -- there is no translation to negotiate on that family
    /// however the address was found. Copy both flags into the signaling
    /// verbatim; the marking is decided here so no application re-derives it.
    bool lan;
} lowlat_candidate_event;

/// Tell the peer this host is ready to be checked.
typedef struct lowlat_ready_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
} lowlat_ready_event;

/// A path was found and media is flowing.
typedef struct lowlat_established_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
    char address[LOWLAT_ADDRESS_MAX];
    uint16_t port;
    uint8_t reserved[2];
} lowlat_established_event;

/// The attempt is over.
typedef struct lowlat_ended_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
    lowlat_outcome outcome;
    /// The status on the ending, from whichever side sent it: what the peer
    /// was told when this host ended it, and what the peer said when it left.
    ///
    /// **A negative value from a peer is the far end reporting its own
    /// fault**, and it is the only account of one there is -- a host cannot
    /// see that a guest failed to decode. Zero otherwise, and zero is not a
    /// status anything stops on.
    int32_t reason;
} lowlat_ended_event;

/// An application message from a guest, or from the host on the client's side.
typedef struct lowlat_user_data_event {
    /// The guest that sent it. **Zero on a client**, whose messages all come
    /// from the host.
    uint32_t guest;
    /// The sub-identifier, which means whatever the application and its
    /// clients agreed it means. Nothing here reads it.
    uint32_t id;
    /// How long the body is. **Not how much was written**: a caller that
    /// offered no buffer is still told what it chose not to receive.
    uint32_t body_len;
} lowlat_user_data_event;

/// What the loop is capturing now.
typedef struct lowlat_capture_changed_event {
    uint32_t width;
    uint32_t height;
    /// The identity of the output being captured, which is what a chooser
    /// marks and what absolute input is expressed against.
    char output[LOWLAT_OUTPUT_MAX];
} lowlat_capture_changed_event;

/// Who holds the pointer now.
typedef struct lowlat_input_owner_event {
    /// `LOWLAT_GUEST_ALL` -- zero -- when nobody holds it.
    uint32_t guest;
} lowlat_input_owner_event;

/// The host cannot continue.
typedef struct lowlat_fatal_event {
    /// What every guest was told on the way out, in the protocol's own
    /// numbering rather than this API's.
    int32_t reason;
} lowlat_fatal_event;

/// The host blocked this client's input, or unblocked it.
typedef struct lowlat_blocked_event {
    bool blocked;
} lowlat_blocked_event;

/// The host ended one stream and left the session up.
typedef struct lowlat_stream_ended_event {
    uint32_t stream;
    /// The status the host gave, in the protocol's own numbering.
    int32_t status;
} lowlat_stream_ended_event;

/// Which mode the host is in.
typedef struct lowlat_host_mode_event {
    uint32_t mode;
} lowlat_host_mode_event;

/// Relative mode entered or left.
///
/// On the way out, where the pointer reappears, in the window's units through
/// the viewport the application set; the application warps its pointer there
/// once, on this transition, and not on every update.
typedef struct lowlat_relative_event {
    bool relative;
    int32_t x;
    int32_t y;
} lowlat_relative_event;

/// Whichever event this is.
///
/// A union cannot describe itself, and the tag beside it is what says which
/// member to read.
typedef union lowlat_event_body {
    lowlat_candidate_event candidate;
    lowlat_ready_event ready;
    lowlat_established_event established;
    lowlat_ended_event ended;
    lowlat_user_data_event user_data;
    lowlat_capture_changed_event capture_changed;
    lowlat_input_owner_event input_owner;
    lowlat_fatal_event fatal;
    lowlat_blocked_event blocked;
    lowlat_stream_ended_event stream_ended;
    lowlat_host_mode_event host_mode;
    lowlat_relative_event relative;
} lowlat_event_body;

/// One event.
///
/// **The tag is first** so an application that does not recognise a type can
/// skip it without knowing anything about the rest, which is what makes adding
/// a type additive.
typedef struct lowlat_event {
    lowlat_event_type kind;
    /// How many events were dropped since the previous delivery.
    ///
    /// **Carried on the next event rather than reported at the time**, which
    /// is the only place it can be: the drop happened because nobody was
    /// polling.
    uint32_t dropped;
    lowlat_event_body body;
} lowlat_event;
#endif

#if defined(LOWLAT_CLIENT)
/// One decoder this machine can open, as `lowlat_enum_decoders` reports
/// it: what creation takes to open exactly this one, and what it decodes.
typedef struct lowlat_decoder_info {
    /// Set by the caller to `sizeof(lowlat_decoder_info)`.
    uint32_t size;
    /// Its position in the enumeration.
    uint32_t index;
    /// One of `lowlat_decoder`, `LOWLAT_DECODER_OPEN` or
    /// `LOWLAT_DECODER_VENDOR`: what `lowlat_client_create_info.decoder`
    /// names to open this one.
    uint32_t decoder;
    /// The largest coded picture per codec, as the device reports it; zero
    /// where it does not say.
    uint32_t max_width_h264;
    uint32_t max_height_h264;
    uint32_t max_width_hevc;
    uint32_t max_height_hevc;
    /// What it decodes. A preference in `lowlat_client_video_config` past
    /// these is masked before anything is declared.
    bool h264;
    bool hevc;
    bool hevc_10;
    bool hevc_444;
    bool hevc_444_10;
    /// Whether it hands pictures out as a handle: what
    /// `lowlat_client_create_info.frame_kind = LOWLAT_FRAME_HANDLE` needs.
    bool handle;
    uint8_t reserved[2];
    /// The render node, NUL-terminated, for `lowlat_client_create_info
    /// .device`; empty for the vendor's device when no node names it, which
    /// creation takes as the first device.
    char device[LOWLAT_OUTPUT_MAX];
    /// The device's or driver's own name, NUL-terminated, for a label.
    char name[LOWLAT_DECODER_NAME_MAX];
} lowlat_decoder_info;

/// What a client is created with.
///
/// **Zeroed is the sensible default**: the first decoder that opens, planes,
/// the largest picture the generation declares.
typedef struct lowlat_client_create_info {
    /// Set by the caller to `sizeof(lowlat_client_create_info)`.
    uint32_t size;
    /// One of `lowlat_decoder`.
    uint32_t decoder;
    /// One of `lowlat_frame_kind`.
    uint32_t frame_kind;
    /// The largest picture the client takes: what its picture slots are
    /// sized for. Zero for the generation's declared maximum, 4096 square.
    /// Nothing is backed until the first picture is decoded.
    uint32_t max_width;
    uint32_t max_height;
    /// The render node the decoder opens, NUL-terminated; empty for the
    /// first that decodes.
    char device[LOWLAT_OUTPUT_MAX];
} lowlat_client_create_info;

/// What a client asks of a host, per attempt.
///
/// What the application would like of the picture, for the one stream.
///
/// **Preferences, not requirements.** Each of the three is "this if the host
/// has it": the library masks them with what its decoder was verified to
/// decode before declaring anything, so a stream the decoder cannot take is
/// never asked for, and follows whatever the host then sends. Zeroed is the
/// sensible default and what every established client asks at its defaults.
typedef struct lowlat_client_video_config {
    /// The picture size asked of the host, or zero for no preference.
    ///
    /// **A request to change the host's display, not a description of this
    /// one.** An established host takes the owner's figure as a mode request,
    /// so set it only to change the person's monitor.
    uint32_t resolution_x;
    uint32_t resolution_y;
    /// The second codec.
    bool hevc;
    /// Ten-bit colour, which implies the second codec.
    bool ten_bit;
    /// Full chroma, which implies the second codec.
    bool chroma_444;
    uint8_t reserved;
} lowlat_client_video_config;

/// **Zeroed is the sensible default**: no size request, no colour
/// preference, compressed sound, the current cipher, no reflexive servers.
typedef struct lowlat_client_config {
    /// Set by the caller to `sizeof(lowlat_client_config)`.
    uint32_t size;
    /// The picture: the size asked of the host and the preferences.
    lowlat_client_video_config video;
    /// Whether uncompressed sound is acceptable.
    bool raw_audio;
    /// Offer no media key, so the host answers without one and both ends key
    /// the legacy 128-bit cipher from its certificate digest.
    bool legacy_cipher;
    /// Offer addresses from the carrier-grade shared range as candidates.
    bool shared_address_space;
    uint8_t reserved;
    /// How many of `servers` are set.
    uint32_t server_count;
    /// Reflexive servers, consulted for this client's own mapped address,
    /// each as `host:port`, NUL-terminated.
    char servers[LOWLAT_SERVERS_MAX][LOWLAT_SERVER_MAX];
} lowlat_client_config;

/// A whole pad at one moment, for `lowlat_client_send_pad_state`.
typedef struct lowlat_pad_state {
    /// Set by the caller to `sizeof(lowlat_pad_state)`.
    uint32_t size;
    /// `LOWLAT_PAD_STATE_*` bits.
    uint16_t buttons;
    int16_t lx;
    int16_t ly;
    int16_t rx;
    int16_t ry;
    uint8_t lt;
    uint8_t rt;
} lowlat_pad_state;

/// The session as it stands.
typedef struct lowlat_client_status {
    /// Set by the caller to `sizeof(lowlat_client_status)`.
    uint32_t size;
    /// `LOWLAT_CLIENT_CONNECTING`, `LOWLAT_CLIENT_ESTABLISHED` or
    /// `LOWLAT_CLIENT_OVER`; `LOWLAT_CLIENT_IDLE` with no attempt.
    uint32_t state;
    /// The status the host's disconnect carried, or zero.
    int32_t disconnect;
    /// The smoothed round trip to the host, in milliseconds.
    uint32_t rtt_ms;
    /// Messages arrived on the video channel and not yet consumed: what the
    /// reader is behind by.
    uint32_t behind;
    /// How long there has been anything unconsumed, in milliseconds.
    uint32_t behind_ms;
    /// Pictures taken off the video channel.
    uint64_t pictures;
    /// Pictures the catch-up discarded to land on a keyframe.
    uint64_t skipped;
    /// Sound packets taken off the audio channel.
    uint64_t audio_packets;
    /// `LOWLAT_DECODER_NONE_YET`, `LOWLAT_DECODER_BUILT` or
    /// `LOWLAT_DECODER_FAILED`.
    uint32_t decoder;
    /// Pictures decoded and published, not yet taken by the application.
    uint32_t queue_depth;
    /// The last picture's decode and read-back, in microseconds.
    uint32_t decode_us;
    uint32_t readback_us;
    /// Pictures decoded.
    uint64_t decoded;
    /// Bytes taken off the video channel, so a rate can be read as a
    /// difference over time.
    uint64_t video_bytes;
    /// The host's own encode time for the stream, as it last reported it,
    /// in microseconds; zero until it has.
    uint32_t encode_us;
    /// The codec the decoder was built for, one of `lowlat_codec`; zero
    /// before a build.
    uint32_t codec;
    /// The decoder backend in use, one of `lowlat_decoder` as resolved at
    /// creation: never `LOWLAT_DECODER_AUTO`.
    uint32_t backend;
    /// Input reports dropped because the session thread was not keeping up.
    /// Nonzero means the loop is not running, not that input is fast.
    uint32_t input_dropped;
    /// Sound packets decoded and handed out by `lowlat_client_acquire_audio`.
    uint64_t audio_decoded;
    /// Sound packets dropped because the application had not taken the
    /// ones before them: the pool holds 32.
    uint32_t audio_dropped;
    /// Sound packets the decoder refused, or that describe a stream this
    /// library does not decode.
    uint32_t audio_refused;
    /// Sound packets taken off the wire and not yet acquired.
    uint32_t audio_queued;
    /// How long the last acquired packet waited between the wire and the
    /// call, in milliseconds.
    uint32_t audio_age_ms;
    /// What the sound decoder was built for: `LOWLAT_AUDIO_OPUS`,
    /// `LOWLAT_AUDIO_PCM`, or zero before a build.
    uint32_t audio_codec;
    /// What the client reports to the host every two seconds: the smoothed
    /// decode and hand-over per picture, and the smoothed decode per sound
    /// packet, in microseconds; zero until something has been timed.
    uint32_t decode_reported_us;
    uint32_t audio_reported_us;
    /// The declaration, in the wire's flag bits: what the application asked
    /// (the preferences as flags, unmasked) and what was declared after the
    /// mask; zero before an attempt.
    uint32_t asked_flags;
    uint32_t declared_flags;
    /// The stream as the decoder built it: one of `LOWLAT_FORMAT_*`, or
    /// zero before a build. With `codec`, what the host turned out to send.
    uint32_t stream_format;
} lowlat_client_status;

/// One plane of a picture.
typedef struct lowlat_plane {
    /// The first sample of the first row, or null for a plane the layout
    /// does not have -- and null for every plane of a frame of the handle
    /// kind, whose planes are `offset` into the handle instead.
    const uint8_t *data;
    /// Bytes from one row to the next.
    uint32_t pitch;
    /// Bytes from the start of the handle to the first sample of the
    /// first row, for a frame of the handle kind; zero otherwise.
    uint64_t offset;
} lowlat_plane;

/// A decoded picture, lent to the application.
///
/// Valid from the acquire that filled it until the release that names it.
/// Every field the renderer needs is here: nothing is read from the stream.
typedef struct lowlat_frame {
    /// Set by the caller to `sizeof(lowlat_frame)`.
    uint32_t size;
    /// One of `lowlat_frame_kind`: what the picture is handed out as.
    uint32_t kind;
    /// One of `LOWLAT_FORMAT_*`.
    uint32_t format;
    uint32_t width;
    uint32_t height;
    /// One of `lowlat_rotation`, applied at present time.
    uint32_t rotation;
    /// The encoder generation the picture belongs to.
    uint32_t generation;
    /// The picture's order in its stream: a later picture has a higher
    /// number, and a gap between two consecutive presents is a skip.
    uint64_t sequence;
    /// Luma, then chroma. A layout with fewer planes leaves the rest null.
    lowlat_plane planes[3];
    /// Which slot this is, for the release.
    uint32_t slot;
    /// One of `lowlat_handle_kind`: none for a frame of the planes kind.
    uint32_t handle_kind;
    /// The descriptor, for `lowlat_handle_kind::LOWLAT_HANDLE_OPAQUE_FD`;
    /// negative otherwise.
    int32_t fd;
    /// The allocation's ordinal since creation, from one, for a frame of
    /// the handle kind. Descriptor numbers are reused once closed, so this
    /// is what tells one allocation from the next: two frames with the
    /// same number share an import, a new number is a new import, and an
    /// import whose number no longer appears may be dropped.
    uint32_t allocation;
    /// The whole allocation behind the descriptor in bytes, which is what
    /// an import is told; zero for a frame of the planes kind.
    uint64_t handle_size;
    /// The layout modifier, for a kind that has one; zero otherwise.
    uint64_t modifier;
} lowlat_frame;

/// A synchronisation object the application's device signals when it has
/// finished reading a picture.
///
/// **None is the only kind in this version**, because every picture leaves
/// as planes that were copied; the shape is fixed so a handle path adds a
/// kind rather than a call.
typedef struct lowlat_fence {
    /// One of `lowlat_fence_kind`.
    uint32_t kind;
    /// The descriptor or handle, as the kind says.
    uint64_t handle;
    /// The value to wait for, as the kind says.
    uint64_t value;
} lowlat_fence;
#endif

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

/// Major and minor, packed.
///
/// **The one function whose signature can never change**, because it is what a
/// loader calls to decide whether it may call anything else.
///
/// @returns The major version in the high sixteen bits, the minor in the low.
uint32_t lowlat_abi_version(void) LOWLAT_NOEXCEPT;

/// Which halves this build of the library carries.
///
/// **Asked rather than probed.** A loader that resolves entry points by name
/// would otherwise learn that a half is missing one unresolved symbol at a
/// time; this says it once, before anything else is looked up. The header
/// hides the same halves under the same names (`LOWLAT_HOST`, `LOWLAT_CLIENT`),
/// so an application compiled for one build cannot name what it lacks.
///
/// @returns `LOWLAT_FEATURE_*` bits, OR-ed.
uint32_t lowlat_features(void) LOWLAT_NOEXCEPT;

/// Describe a status.
///
/// **It takes a plain integer rather than the enumeration**, so that a value
/// from anywhere can be described -- including one this version of the library
/// does not define, which is exactly the case an application reaches for this
/// in. Passing a status to it is an ordinary widening conversion.
///
/// The pointer is to storage that outlives the library, so it is never freed
/// and never copied out of.
///
/// @param[in] status Any status value, including one this version does not define.
/// @returns A NUL-terminated description. Never null, never freed.
const char *lowlat_status_string(int32_t status) LOWLAT_NOEXCEPT;

/// Receive log messages from every part of this library.
///
/// Passing `NULL` stops delivery and returns the library to writing lines on
/// standard error itself.
///
/// **The callback may be replaced.** The underlying sink is process-wide and
/// installed once; what an application registers here sits behind it, so
/// calling this again changes where lines go rather than being refused.
///
/// @param[in] fn_ Where lines go, or `NULL` to return them to standard error.
/// @param[in] opaque Handed back to `fn_` untouched.
/// @returns `LOWLAT_OK`.
///
/// @attention `fn_` must remain callable, and `opaque` valid, until this is called
/// again with something else or with `NULL`. It may fire on any thread, and it must not
/// call back into this library.
lowlat_status lowlat_set_log_callback(lowlat_log_fn fn_,
                                      void *opaque) LOWLAT_NOEXCEPT;

/// Set how much is logged. Lines above this level are not formatted at all.
///
/// @param[in] level One of `lowlat_log_level`.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_INVALID_ARGUMENT` for a level nothing
/// defines.
lowlat_status lowlat_set_log_level(uint32_t level) LOWLAT_NOEXCEPT;

#if defined(LOWLAT_HOST)
/// A configuration filled with what a host would choose for itself.
///
/// **Zero is not a configuration.** Every enumerated field here is validated
/// rather than clamped, so a structure the caller zeroed is a *valid* request
/// for the first variant of everything -- H.264, the most aggressive
/// congestion level -- and the boundary cannot tell that apart from an
/// application that meant it. Starting from this, and overwriting what the
/// application actually has an opinion about, is what keeps an unset field
/// unset rather than accidentally set to zero.
///
/// `size` is filled in, so a caller that starts here does not have to know it
/// exists. Each field's own default is on the field.
lowlat_host_config lowlat_host_config_default(void) LOWLAT_NOEXCEPT;

/// Create a handle.
///
/// @param[in] info One `lowlat_host_create_info` whose `size` says how much of it is set.
/// May be null, which takes every default.
/// @param[out] out Receives the handle.
/// @returns `LOWLAT_OK`, or an error and `out` left untouched.
///
/// @attention `out` must point to storage for one pointer. `info` may be null, which
/// takes every default.
lowlat_status lowlat_host_create(const lowlat_host_create_info *info,
                                 lowlat_host **out) LOWLAT_NOEXCEPT;

/// Destroy a handle.
///
/// **Works on a poisoned handle**, which is the point of poisoning: everything
/// else is refused and this still releases what was taken.
///
/// @param[in] hl The handle from `lowlat_host_create`, not used again. Null is accepted
/// and does nothing.
///
/// @attention `hl` came from `lowlat_host_create` and is not used again. A null pointer is
/// accepted and does nothing.
void lowlat_host_destroy(lowlat_host *hl) LOWLAT_NOEXCEPT;

/// Start hosting.
///
/// Guests are admitted through the signaling seam, which is the application's
/// own; this starts what serves them once they arrive.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] cfg One `lowlat_host_config` whose `size` says how much of it is set.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_ALREADY_STARTED` when this handle is
/// already hosting.
///
/// @attention `hl` came from `lowlat_host_create`, and `cfg` points to one
/// `lowlat_host_config` whose `size` says how much of it is set.
lowlat_status lowlat_host_start(lowlat_host *hl,
                                const lowlat_host_config *cfg) LOWLAT_NOEXCEPT;

/// Register an attempt from an offer signaling delivered.
///
/// **Registering is not approving.** This takes a seat's worth of bookkeeping
/// and nothing else; no socket is opened and no thread is started until
/// `lowlat_host_begin_p2p`. An application that decides to decline simply
/// never calls that, and says so over its own signaling.
///
/// `LOWLAT_ERR_AT_CAPACITY` means the offer should be declined rather than
/// left unanswered: nothing in the protocol reports a host that never replied,
/// so a peer given silence sits connecting until its own deadline.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] info One `lowlat_attempt_info` whose `size` says how much of it is set.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_AT_CAPACITY` when the room is full -- which
/// the application should decline over its own signaling rather than leave unanswered.
///
/// @attention `hl` came from `lowlat_host_create`, and `info` points to one
/// `lowlat_attempt_info` whose `size` says how much of it is set.
lowlat_status lowlat_host_new_attempt(lowlat_host *hl,
                                      const lowlat_attempt_info *info) LOWLAT_NOEXCEPT;

/// Offer one address the peer might be reachable at.
///
/// **An unknown attempt is accepted silently.** Candidates trickle and a
/// withdrawal can overtake them, so this is a race with teardown rather than a
/// fault, and a status the caller would have to ignore is worse than no status.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] attempt_id The attempt this address belongs to, NUL-terminated. One
/// nothing registered is accepted silently.
/// @param[in] cand One `lowlat_candidate`.
///
/// @attention `hl` came from `lowlat_host_create`, `attempt_id` is a NUL-terminated
/// string, and `cand` points to one `lowlat_candidate`.
void lowlat_host_add_candidate(lowlat_host *hl,
                               const char *attempt_id,
                               const lowlat_candidate *cand) LOWLAT_NOEXCEPT;

/// Approve an attempt and answer it with this host's own credentials.
///
/// This is where a socket is opened and this guest's threads are started, so
/// it is the one call in the seam that costs more than bookkeeping. It sends
/// nothing: the answer travels over the application's signaling, because this
/// library has no transport for it.
///
/// `port` is where the bind **starts**, not where it must land. It walks when
/// the port is taken and the port it reached comes back in `out`, so the two
/// are an in and an out pair rather than one value asked and assumed. **Zero
/// asks for the configured base port**, which is what a caller with no opinion
/// passes; a caller with an opinion has one for a reason -- a mapping on the
/// gateway, a rule on the firewall, a pool it allocates from -- and none of
/// those survive this library choosing for it.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] attempt_id The attempt to approve, NUL-terminated.
/// @param[in] port Where the bind starts, not where it must land. Zero asks for the
/// configured base port.
/// @param[out] out Receives this host's credentials and the port the bind reached, in
/// one `lowlat_credentials` whose `size` says how much of it is set.
/// @returns `LOWLAT_OK`, and the application sends `out` to the peer over its own
/// signaling.
///
/// @attention `hl` came from `lowlat_host_create`, `attempt_id` is a NUL-terminated
/// string, and `out` points to one `lowlat_credentials` whose `size` says how much of
/// it is set.
lowlat_status lowlat_host_begin_p2p(lowlat_host *hl,
                                    const char *attempt_id,
                                    uint16_t port,
                                    lowlat_credentials *out) LOWLAT_NOEXCEPT;

/// End an attempt, whether or not it was ever approved.
///
/// **An unknown identifier is accepted silently**, and remembered: a
/// withdrawal can arrive before the offer it withdraws, and admitting that
/// offer afterwards spends a socket and a thread on a guest that has already
/// gone.
///
/// **The peer is not told why.** Ending stops this guest's loop; the far side
/// learns from its own liveness deadline rather than from a message, for the
/// same reason `lowlat_host_stop` does.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] attempt_id The attempt to end, NUL-terminated. One nothing registered is
/// accepted silently and remembered.
///
/// @attention `hl` came from `lowlat_host_create` and `attempt_id` is a NUL-terminated
/// string.
void lowlat_host_end_connection(lowlat_host *hl,
                                const char *attempt_id) LOWLAT_NOEXCEPT;

/// List the guests that are connected.
///
/// **Two calls, and the caller owns the buffer.** Pass `NULL` for `out` to
/// learn how many there are, then an array of that many. Nothing here is
/// allocated on the application's behalf, so there is nothing to free.
///
/// `count` carries the array's capacity in and the number written out. A
/// buffer smaller than the roster is filled as far as it goes and answered
/// with `LOWLAT_ERR_TOO_SMALL`, `count` set to what it would have taken --
/// the roster moves, and a caller that sized its array a moment ago must not
/// be made to lose the call.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[out] out An array of at least `*count` entries, or `NULL` to ask only how
/// many there are.
/// @param[in,out] count The array's capacity in, the number written out.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_TOO_SMALL` with `count` set to what the
/// roster would have taken.
///
/// @attention `count` must be readable and writable, and `out`, when not null, must
/// point to at least `*count` elements.
lowlat_status lowlat_host_get_guests(lowlat_host *hl,
                                     lowlat_guest *out,
                                     uint32_t *count) LOWLAT_NOEXCEPT;

/// Send one guest an application message, or every guest at once.
///
/// **Nothing here reads the body.** The sub-identifier and the bytes are an
/// agreement between an application and the clients it serves; a host that
/// interpreted either would be inventing a protocol on its behalf
/// ([05 §5](../docs/05-host.md)).
///
/// `guest_id` of `LOWLAT_GUEST_ALL` reaches everyone seated. A body past
/// what a peer will accept is refused here rather than sent and dropped in
/// silence at the far end.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] guest_id Which guest, or `LOWLAT_GUEST_ALL` for everyone seated.
/// @param[in] id The sub-identifier, which means whatever the application and its
/// clients agreed it means.
/// @param[in] data The body, copied before this returns and never retained.
/// @param[in] len How long the body is. A body past what a peer will accept is refused
/// here.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_UNKNOWN_GUEST`.
///
/// @attention `data` must point to at least `len` bytes when `len` is not zero. It is
/// copied before the call returns and never retained.
lowlat_status lowlat_host_send_user_data(lowlat_host *hl,
                                         uint32_t guest_id,
                                         uint32_t id,
                                         const void *data,
                                         uint32_t len) LOWLAT_NOEXCEPT;

/// End one guest, telling it why.
///
/// **`reason` is not a `lowlat_status`.** It reaches the peer as the
/// protocol's own disconnect status, which is a different numbering that
/// happens to share a width. **Zero is not a value to pass**: a peer carries on
/// through it, so a guest kicked with zero is told nothing and stays.
///
/// The guest is sent the reason, given a moment for it to arrive, and then its
/// seat goes back. It does not disappear from the roster the instant this
/// returns.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] guest_id Which guest to end.
/// @param[in] reason What the peer is told, in the protocol's own disconnect numbering
/// rather than this API's. Zero tells it nothing and leaves it seated.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_UNKNOWN_GUEST`.
///
/// @attention `hl` came from `lowlat_host_create`.
lowlat_status lowlat_host_kick_guest(lowlat_host *hl,
                                     uint32_t guest_id,
                                     int32_t reason) LOWLAT_NOEXCEPT;

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
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] guest_id Which guest.
/// @param[in] perms One `lowlat_permissions`. Every flag clear is how a guest's input
/// is turned off.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_UNKNOWN_GUEST`.
///
/// @attention `hl` came from `lowlat_host_create`, and `perms` points to one
/// `lowlat_permissions`.
lowlat_status lowlat_host_set_permissions(lowlat_host *hl,
                                          uint32_t guest_id,
                                          const lowlat_permissions *perms) LOWLAT_NOEXCEPT;

/// List the sound outputs this host could capture.
///
/// **Available before hosting starts**, and it does not disturb a host that is
/// running: it asks over a connection of its own. Two calls and the caller's
/// own buffer, like the video one.
///
/// A machine with no sound server answers with none rather than failing, which
/// is the same thing an application does with it: offer what there is.
///
/// @param[out] out An array of at least `*count` entries, or `NULL` to ask only how
/// many there are.
/// @param[in,out] count The array's capacity in, the number written out.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_TOO_SMALL` with `count` set to what it
/// would have taken.
///
/// @attention `count` must be readable and writable, and `out`, when not null, must
/// point to at least `*count` elements.
lowlat_status lowlat_get_audio_outputs(lowlat_audio_output *out,
                                       uint32_t *count) LOWLAT_NOEXCEPT;

/// List the outputs this host could capture.
///
/// **Available before hosting starts**, so an application can present a choice
/// before committing to one. Two calls and the caller's own buffer, like the
/// roster: pass `NULL` to learn the count.
///
/// @param[out] out An array of at least `*count` entries, or `NULL` to ask only how
/// many there are.
/// @param[in,out] count The array's capacity in, the number written out.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_TOO_SMALL` with `count` set to what it
/// would have taken.
///
/// @attention `count` must be readable and writable, and `out`, when not null, must
/// point to at least `*count` elements.
lowlat_status lowlat_get_outputs(lowlat_output *out,
                                 uint32_t *count) LOWLAT_NOEXCEPT;

/// Whether this machine could host right now.
///
/// **A pre-flight, and the reason it exists is that the two ways of failing
/// look identical afterwards.** Starting a host that cannot capture fails deep
/// in the stream loop, where an application can tell "there is no display"
/// from "this process may not read one" only by reading a log. This answers
/// which, before anything is started.
///
/// `LOWLAT_OK` means a display is lit and its framebuffer can be reached.
/// It is a read: no encoder is built and no thread is started.
///
/// @returns `LOWLAT_OK` when a display is lit and its framebuffer can be reached,
/// `LOWLAT_ERR_NO_DISPLAY` when there is none, and `LOWLAT_ERR_DISPLAY_UNREACHABLE`
/// when this process may not read the one there is.
lowlat_status lowlat_can_host(void) LOWLAT_NOEXCEPT;

/// Read what the host is doing.
///
/// Answers on a handle that is not hosting too, with `running` clear: an
/// application asking what state something is in should not have to know the
/// answer first.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[out] out One `lowlat_host_status` whose `size` says how much of it is set.
/// @returns `LOWLAT_OK`, on a handle that is not hosting too.
///
/// @attention `out` points to one `lowlat_host_status` whose `size` says how much of
/// it is set.
lowlat_status lowlat_host_get_status(lowlat_host *hl,
                                     lowlat_host_status *out) LOWLAT_NOEXCEPT;

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
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] data The body, whose shape belongs to the clients the application serves.
/// Copied before this returns and never retained.
/// @param[in] len How long the body is.
/// @param[out] reached How many guests it reached. Zero for an empty room, which is not
/// an error.
/// @returns `LOWLAT_OK`.
///
/// @attention `data` must point to at least `len` bytes when `len` is not zero. It is
/// copied before the call returns and never retained.
lowlat_status lowlat_host_send_roster(lowlat_host *hl,
                                      const void *data,
                                      uint32_t len,
                                      uint32_t *reached) LOWLAT_NOEXCEPT;

/// Read what one guest is doing.
///
/// **What this host can answer for, and nothing else.** A peer's own decode
/// time and how many frames it has queued waiting to decode are the peer's to
/// know; reporting either would be reporting a number this host made up.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] guest_id Which guest.
/// @param[out] out One `lowlat_metrics` whose `size` says how much of it is set.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_UNKNOWN_GUEST`.
///
/// @attention `out` points to one `lowlat_metrics` whose `size` says how much of it
/// is set.
lowlat_status lowlat_host_get_metrics(lowlat_host *hl,
                                      uint32_t guest_id,
                                      lowlat_metrics *out) LOWLAT_NOEXCEPT;

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
/// Refused with `LOWLAT_ERR_INVALID_ARGUMENT` when the host is not running,
/// because there is nothing yet for the values to apply to and accepting them
/// silently would report settings that never took.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] cfg One `lowlat_host_video_config` whose `size` says how much of it is
/// set.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_INVALID_ARGUMENT` when the host is not
/// running.
///
/// @attention `hl` came from `lowlat_host_create`, and `cfg` points to one
/// `lowlat_host_video_config` whose `size` says how much of it is set.
lowlat_status lowlat_host_set_video_config(lowlat_host *hl,
                                           const lowlat_host_video_config *cfg) LOWLAT_NOEXCEPT;

/// Change what sound is set to, while a host runs.
///
/// **Every field takes effect without a restart.** Switching sound off gives
/// the device back and restores the speakers; switching it on takes it again.
/// A device that does not resolve is refused rather than substituted, and the
/// host keeps the one it has.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] cfg One `lowlat_host_audio_config` whose `size` says how much of it is
/// set.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_INVALID_ARGUMENT` for a device that does
/// not resolve, the host keeping the one it has.
///
/// @attention `hl` came from `lowlat_host_create`, and `cfg` points to one
/// `lowlat_host_audio_config` whose `size` says how much of it is set.
lowlat_status lowlat_host_set_audio_config(lowlat_host *hl,
                                           const lowlat_host_audio_config *cfg) LOWLAT_NOEXCEPT;

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
/// `lowlat_host_status`.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[out] out One `lowlat_host_audio_config` whose `size` says how much of it is
/// set.
/// @returns `LOWLAT_OK`.
///
/// @attention `hl` came from `lowlat_host_create`, and `out` points to one
/// `lowlat_host_audio_config` whose `size` says how much of it is set.
lowlat_status lowlat_host_get_audio_config(lowlat_host *hl,
                                           lowlat_host_audio_config *out) LOWLAT_NOEXCEPT;

/// What the host is running at now.
///
/// **Read back rather than remembered.** What a stream is doing is the
/// stream's answer, and an application that kept its own copy would be
/// describing settings another guest may have changed underneath it.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[out] out One `lowlat_host_video_config` whose `size` says how much of it is
/// set.
/// @returns `LOWLAT_OK`.
///
/// @attention `hl` came from `lowlat_host_create`, and `out` points to one
/// `lowlat_host_video_config` whose `size` says how much of it is set.
lowlat_status lowlat_host_get_video_config(lowlat_host *hl,
                                           lowlat_host_video_config *out) LOWLAT_NOEXCEPT;

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
/// @param[in] hl The handle from `lowlat_host_create`. It may be started again.
/// @returns `LOWLAT_OK`, once every guest is disconnected and every thread joined.
///
/// @attention `hl` came from `lowlat_host_create`.
lowlat_status lowlat_host_stop(lowlat_host *hl) LOWLAT_NOEXCEPT;

/// Take one packet of a guest's microphone, waiting up to `timeout_ms`.
///
/// **Its own poll, not the event queue.** A hundred packets a second sharing
/// that queue would evict the events it is there to deliver, so sound has a
/// queue of its own and an application that wants both polls both.
///
/// **Always samples, never a codec.** A guest chooses how it encodes and this
/// library decodes whichever it chose: sixteen-bit, mono, at
/// `LOWLAT_MICROPHONE_SAMPLE_RATE`. `samples` must hold
/// `LOWLAT_MICROPHONE_SAMPLES_MAX` of them; a packet cannot be larger, so
/// there is no partial delivery and nothing to call back for.
///
/// Answers `LOWLAT_TIMEOUT` when nothing arrived, which is not an error, and
/// `LOWLAT_ERR_NOT_STARTED` when this host is not taking microphones: it
/// does nothing in that case rather than waiting out a timeout for sound that
/// by construction cannot come. Set `accept_microphone` in
/// `lowlat_host_audio_config` to take one; it is off by default, and until
/// it is on a peer keeps its microphone muted and sends nothing.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] timeout_ms How long to wait for a packet. Zero polls without waiting.
/// @param[out] samples Where the packet is written. Must hold
/// `LOWLAT_MICROPHONE_SAMPLES_MAX`.
/// @param[in,out] count The buffer's capacity in samples in, how many were written out.
/// @param[out] guest Which guest sent it. May be null.
/// @param[out] dropped How many packets were lost to a queue nobody was draining,
/// reported with the next delivery. May be null.
/// @returns `LOWLAT_OK`, `LOWLAT_TIMEOUT` when nothing arrived, or
/// `LOWLAT_ERR_NOT_STARTED` when this host is not taking microphones.
///
/// @attention `samples` points to at least `*count` samples, and `count`, `guest` and
/// `dropped` are readable and writable. `guest` and `dropped` may be null.
lowlat_status lowlat_host_poll_microphone(lowlat_host *hl,
                                          uint32_t timeout_ms,
                                          int16_t *samples,
                                          uint32_t *count,
                                          uint32_t *guest,
                                          uint32_t *dropped) LOWLAT_NOEXCEPT;

/// Take one event, waiting up to `timeout_ms` for one to arrive.
///
/// Answers `LOWLAT_TIMEOUT` when nothing arrived, which is not an error. A
/// `timeout_ms` of zero polls without waiting.
///
/// `body` receives an application message's body and may be null, which means
/// the application does not want bodies: one that arrives is delivered without
/// it, and the event still says how long it was. When `body` is not null,
/// `body_len` carries its capacity in and the bytes written out.
///
/// **A body that does not fit consumes nothing.** `LOWLAT_ERR_TOO_SMALL` is
/// answered, `body_len` is set to what the body needs, and the same event is
/// delivered by the next call with room for it.
///
/// @param[in] hl The handle from `lowlat_host_create`.
/// @param[in] timeout_ms How long to wait for an event. Zero polls without waiting.
/// @param[out] out One `lowlat_event`.
/// @param[out] body Receives an application message's body, or `NULL` to be delivered
/// events without their bodies.
/// @param[in,out] body_len When `body` is not null, its capacity in and the bytes
/// written out.
/// @returns `LOWLAT_OK`, `LOWLAT_TIMEOUT` when nothing arrived, or
/// `LOWLAT_ERR_TOO_SMALL` with `body_len` set to what the body needs and the event
/// kept for the next call.
///
/// @attention `out` must point to one `lowlat_event`. `body`, when not null, must
/// point to at least `*body_len` bytes, and `body_len` must then be readable and
/// writable.
lowlat_status lowlat_host_poll_events(lowlat_host *hl,
                                      uint32_t timeout_ms,
                                      lowlat_event *out,
                                      void *body,
                                      uint32_t *body_len) LOWLAT_NOEXCEPT;

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
/// @param[in] hl The handle from `lowlat_host_create`. It is poisoned afterwards: every
/// later call on it is refused and destroying it still works.
/// @returns `LOWLAT_ERR_INTERNAL`, the panic having been caught. Every later call on
/// `hl` answers `LOWLAT_ERR_POISONED`.
///
/// @attention `hl` came from `lowlat_host_create`.
lowlat_status lowlat_debug_panic(lowlat_host *hl) LOWLAT_NOEXCEPT;
#endif

#if defined(LOWLAT_CLIENT)
/// The `index`-th decoder this machine can open, in a fixed order: the
/// open decoder on each render node that decodes, then the vendor's on
/// each of its devices. Callers iterate from zero until this returns
/// false. Each call probes the devices afresh, a few milliseconds, so it
/// is for a startup or a settings screen, not a loop.
///
/// A row is opened by creation with its `decoder` and `device`, and
/// `frame_kind = LOWLAT_FRAME_HANDLE` on a row whose `handle` is set.
///
/// @param[in] index The position, from zero.
/// @param[out] out One `lowlat_decoder_info` with `size` set, filled when
/// there is a decoder at `index`.
/// @returns True with `out` filled; false past the last decoder, or when
/// `out` is null or its `size` is short.
///
/// @attention `out` is null or points to one `lowlat_decoder_info` whose `size` is
/// set.
bool lowlat_enum_decoders(uint32_t index,
                          lowlat_decoder_info *out) LOWLAT_NOEXCEPT;

/// Create a handle.
///
/// @param[in] info One `lowlat_client_create_info` whose `size` says how much of it is set.
/// May be null, which takes every default.
/// @param[out] out Receives the handle.
/// @returns `LOWLAT_OK`, or an error and `out` left untouched.
///
/// @attention `out` must point to storage for one pointer. `info` may be null.
lowlat_status lowlat_client_create(const lowlat_client_create_info *info,
                                   lowlat_client **out) LOWLAT_NOEXCEPT;

/// Destroy a handle, leaving any session it holds.
///
/// **Works on a poisoned handle**, which is the point of poisoning.
///
/// @param[in] cl The handle from `lowlat_client_create`, not used again. Null is accepted
/// and does nothing.
///
/// @attention `cl` came from `lowlat_client_create` and is not used again.
void lowlat_client_destroy(lowlat_client *cl) LOWLAT_NOEXCEPT;

/// Begin an attempt: mint the credentials the offer carries.
///
/// **Nothing is sent and no socket is opened.** The application puts what
/// comes back into its offer over its own signaling and calls
/// `lowlat_client_begin_p2p` with the answer. One attempt at a time; a second
/// while one exists is refused with `LOWLAT_ERR_ALREADY_STARTED`.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] cfg What to ask of the host. May be null, which takes every default.
/// @param[in] attempt_id The identifier every later call and event names, NUL-terminated.
/// @param[in] transport A `lowlat_transport` value. Only the native one is accepted.
/// @param[out] ours Filled with the credentials for the offer. `port` is zero: the socket
/// does not exist yet.
/// @returns `LOWLAT_OK`, `LOWLAT_ERR_ALREADY_STARTED`, `LOWLAT_ERR_INVALID_ARGUMENT`
/// for the browser pipe or a server that does not resolve, or `LOWLAT_ERR_CRYPTO`.
///
/// @attention `cl` came from `lowlat_client_create`; `attempt_id` is NUL-terminated;
/// `cfg` is null or points to one `lowlat_client_config` whose `size` says
/// how much of it is set; `ours` points to one `lowlat_credentials` whose
/// `size` is set.
lowlat_status lowlat_client_new_attempt(lowlat_client *cl,
                                        const lowlat_client_config *cfg,
                                        const char *attempt_id,
                                        uint32_t transport,
                                        lowlat_credentials *ours) LOWLAT_NOEXCEPT;

/// Offer one address the host might be reachable at.
///
/// **An unknown attempt is accepted silently**: a candidate can arrive after
/// the attempt was ended, and that is a race rather than a fault.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] attempt_id The attempt this address belongs to, NUL-terminated.
/// @param[in] cand The candidate, `size` set.
///
/// @attention `cl` came from `lowlat_client_create`; `attempt_id` is NUL-terminated;
/// `cand` points to one `lowlat_candidate` whose `size` is set.
void lowlat_client_add_candidate(lowlat_client *cl,
                                 const char *attempt_id,
                                 const lowlat_candidate *cand) LOWLAT_NOEXCEPT;

/// The answer arrived: bind, start connectivity, and begin trickling
/// candidates as events.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] attempt_id The attempt the answer is for, NUL-terminated.
/// @param[in] theirs The host's credentials from the answer, `size` set. An empty `aes256`
/// selects the legacy cipher, keyed from `fingerprint`.
/// @returns `LOWLAT_OK`, `LOWLAT_ERR_UNKNOWN_ATTEMPT`, `LOWLAT_ERR_ALREADY_BEGUN`,
/// `LOWLAT_ERR_INVALID_ARGUMENT` for credentials that cannot key a session,
/// `LOWLAT_ERR_IO` or `LOWLAT_ERR_CRYPTO`.
///
/// @attention `cl` came from `lowlat_client_create`; `attempt_id` is NUL-terminated;
/// `theirs` points to one `lowlat_credentials` whose `size` is set.
lowlat_status lowlat_client_begin_p2p(lowlat_client *cl,
                                      const char *attempt_id,
                                      const lowlat_credentials *theirs) LOWLAT_NOEXCEPT;

/// Leave the session, or abandon an attempt that never began.
///
/// A session that is up is told, on the control channel, that this client is
/// leaving cleanly; the message is given a moment to arrive. **No event is
/// raised**: the application caused this. A handle with no attempt is left as
/// it is.
///
/// @param[in] cl The handle from `lowlat_client_create`.
///
/// @attention `cl` came from `lowlat_client_create`.
void lowlat_client_end_connection(lowlat_client *cl) LOWLAT_NOEXCEPT;

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
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] x The rectangle's left edge in the window.
/// @param[in] y Its top edge.
/// @param[in] w Its width; zero for no picture area.
/// @param[in] h Its height.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_set_viewport(lowlat_client *cl,
                                         int32_t x,
                                         int32_t y,
                                         int32_t w,
                                         int32_t h) LOWLAT_NOEXCEPT;

/// Change what the application would like of the picture, mid-session.
///
/// The new declaration is masked by capability as at the attempt and
/// restated to the host with a reinitialisation request; the decoder is torn
/// down with it, so the next keyframe builds one for whatever the host now
/// sends. Costs the host one keyframe, and an established host an encoder
/// rebuild, so it is for a person changing a setting rather than a loop. The
/// size request travels with it.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] video The preferences, whole.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_UNKNOWN_ATTEMPT` with no attempt
/// to apply them to.
///
/// @attention `cl` came from `lowlat_client_create`; `video` points at a readable
/// structure.
lowlat_status lowlat_client_set_video_config(lowlat_client *cl,
                                             const lowlat_client_video_config *video) LOWLAT_NOEXCEPT;

/// A key, by the usage code of the physical key. A code of zero is no key
/// and is not sent.
///
/// The rules every client applies are the library's (docs/10-client.md
/// section 8), here and in the calls below, and **none of them blocks**:
/// reports cross a fixed ring to the session thread, and a ring that fills --
/// a thread that is not running -- drops the newest and counts it in
/// `lowlat_client_status`.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] code The usage code.
/// @param[in] mods `LOWLAT_MOD_*` bits in effect, lock state included.
/// @param[in] pressed Down or up.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_key(lowlat_client *cl,
                                     uint32_t code,
                                     uint32_t mods,
                                     bool pressed) LOWLAT_NOEXCEPT;

/// A mouse button, with where the pointer was in the window's units. A press
/// outside the picture's rectangle is not sent; a release always is.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] button One of `LOWLAT_MOUSE_*`.
/// @param[in] pressed Down or up.
/// @param[in] x Where the pointer was.
/// @param[in] y Where the pointer was.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_mouse_button(lowlat_client *cl,
                                              uint32_t button,
                                              bool pressed,
                                              int32_t x,
                                              int32_t y) LOWLAT_NOEXCEPT;

/// Wheel movement, 120 to a detent, positive away from the hand.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] x Sideways.
/// @param[in] y Up and down.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_mouse_wheel(lowlat_client *cl,
                                             int32_t x,
                                             int32_t y) LOWLAT_NOEXCEPT;

/// A pointer position in the window's units, mapped into the picture through
/// the viewport; or a delta when relative, scaled by the picture's size
/// against the viewport's. An absolute position before a viewport is set is
/// not sent.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] x The position, or the delta.
/// @param[in] y The position, or the delta.
/// @param[in] relative A delta rather than a position.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_mouse_motion(lowlat_client *cl,
                                              int32_t x,
                                              int32_t y,
                                              bool relative) LOWLAT_NOEXCEPT;

/// One button of a pad. The pad identifier is the application's and is
/// arbitrary; a host maps it to a slot.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] pad The pad.
/// @param[in] button One of `LOWLAT_PAD_*`, the index form.
/// @param[in] pressed Down or up.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_pad_button(lowlat_client *cl,
                                            uint32_t pad,
                                            uint32_t button,
                                            bool pressed) LOWLAT_NOEXCEPT;

/// One axis of a pad.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] pad The pad.
/// @param[in] axis One of `LOWLAT_PAD_AXIS_*`.
/// @param[in] value The position: a stick over the signed range, a trigger from zero.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_pad_axis(lowlat_client *cl,
                                          uint32_t pad,
                                          uint32_t axis,
                                          int16_t value) LOWLAT_NOEXCEPT;

/// A whole pad at once. An unchanged state for the same pad is not sent
/// again.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] pad The pad.
/// @param[in] state The state, its `size` set.
/// @returns `LOWLAT_OK`, `LOWLAT_ERR_NOT_STARTED` with no session up, or
/// `LOWLAT_ERR_INVALID_ARGUMENT`.
///
/// @attention `cl` came from `lowlat_client_create`; `state` points to one
/// `lowlat_pad_state` whose `size` is set.
lowlat_status lowlat_client_send_pad_state(lowlat_client *cl,
                                           uint32_t pad,
                                           const lowlat_pad_state *state) LOWLAT_NOEXCEPT;

/// The pad is gone. The host destroys its device, which releases everything.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] pad The pad.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_pad_unplug(lowlat_client *cl,
                                            uint32_t pad) LOWLAT_NOEXCEPT;

/// Everything held comes up on the host; sent on losing focus. Pads are
/// centred by it, not unplugged.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_NOT_STARTED` with no session up.
///
/// @attention `cl` came from `lowlat_client_create`.
lowlat_status lowlat_client_send_release_all(lowlat_client *cl) LOWLAT_NOEXCEPT;

/// Send the host's application a message.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] id The sub-identifier, which means whatever the two applications agreed.
/// @param[in] data The body. A terminator is added; one already there is not doubled.
/// @param[in] len How many bytes of `data`.
/// @returns `LOWLAT_OK`, `LOWLAT_ERR_NOT_STARTED` with no session up, or
/// `LOWLAT_ERR_INVALID_ARGUMENT` past the message ceiling.
///
/// @attention `cl` came from `lowlat_client_create`; `data` points to `len` readable
/// bytes, or is null with `len` zero.
lowlat_status lowlat_client_send_user_data(lowlat_client *cl,
                                           uint32_t id,
                                           const void *data,
                                           uint32_t len) LOWLAT_NOEXCEPT;

/// Where the session stands.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[out] out One `lowlat_client_status` with `size` set, filled.
/// @returns `LOWLAT_OK`, or `LOWLAT_ERR_INVALID_ARGUMENT`.
///
/// @attention `cl` came from `lowlat_client_create`; `out` points to one
/// `lowlat_client_status` whose `size` is set.
lowlat_status lowlat_client_get_status(lowlat_client *cl,
                                       lowlat_client_status *out) LOWLAT_NOEXCEPT;

/// Take the newest picture, waiting up to `timeout_ms` for one newer than
/// the last one taken.
///
/// **Acquire is the poll.** Older pictures that were ready are discarded on
/// the way: the newest is what a renderer wants, and a picture it never
/// looked at is the one nothing will miss. A picture stays valid until it is
/// released and may be presented as often as the application likes in
/// between. At most two are held at once -- the one being presented and the
/// one just acquired, so a swap has no gap -- and a third acquire is refused
/// with `LOWLAT_ERR_TOO_MANY_HELD` rather than dropping one silently.
///
/// **Outside the handle's lock**, like the event poll: a wait here leaves
/// every other call answerable.
///
/// @param[in] cl The handle.
/// @param[in] stream The stream, zero in this version.
/// @param[in] timeout_ms How long to wait. Zero polls.
/// @param[out] frame The picture, when `LOWLAT_OK`.
/// @returns `LOWLAT_OK`, `LOWLAT_TIMEOUT` with nothing newer in time,
/// `LOWLAT_ERR_TOO_MANY_HELD`, `LOWLAT_ERR_NOT_STARTED` with no session, or
/// `LOWLAT_ERR_INVALID_ARGUMENT`.
///
/// @attention `cl` came from `lowlat_client_create`; `frame` points to one
/// `lowlat_frame` whose `size` is set.
lowlat_status lowlat_client_acquire_frame(lowlat_client *cl,
                                          uint8_t stream,
                                          uint32_t timeout_ms,
                                          lowlat_frame *frame) LOWLAT_NOEXCEPT;

/// Give a picture back.
///
/// @param[in] cl The handle.
/// @param[in] frame The picture, as acquired.
/// @param[in] done A fence the application's device signals when it has
/// finished reading, or null for reusable now. **Null is the only value this
/// version takes**: every picture leaves as copied planes.
/// @returns `LOWLAT_OK` or `LOWLAT_ERR_INVALID_ARGUMENT`.
///
/// @attention `cl` came from `lowlat_client_create`; `frame` points to a
/// `lowlat_frame` an acquire filled; `done` is null or points to one
/// `lowlat_fence`.
lowlat_status lowlat_client_release_frame(lowlat_client *cl,
                                          const lowlat_frame *frame,
                                          const lowlat_fence *done) LOWLAT_NOEXCEPT;

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
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] timeout_ms How long to wait. Zero polls.
/// @param[out] samples Room for `*count` frames of two samples each.
/// @param[in,out] count How many frames there is room for; on return, how
/// many were written, or how many the waiting packet needs.
/// @returns `LOWLAT_OK`, `LOWLAT_TIMEOUT`, `LOWLAT_ERR_TOO_SMALL` with
/// the need in `*count` and the packet kept for the next call,
/// `LOWLAT_ERR_NOT_STARTED` with no session, or
/// `LOWLAT_ERR_INVALID_ARGUMENT`.
///
/// @attention `cl` came from `lowlat_client_create`; `samples` points to at least
/// `2 * *count` values; `count` points to one `uint32_t`.
lowlat_status lowlat_client_acquire_audio(lowlat_client *cl,
                                          uint32_t timeout_ms,
                                          int16_t *samples,
                                          uint32_t *count) LOWLAT_NOEXCEPT;

/// Take the next event, waiting up to `timeout_ms` for one.
///
/// The shape is `lowlat_host_poll_events`'s: a user-data body is copied into
/// `body` when one is offered, and one that does not fit is left where it was
/// with the length it needed written back.
///
/// @param[in] cl The handle from `lowlat_client_create`.
/// @param[in] timeout_ms How long to wait. Zero polls.
/// @param[out] out The event.
/// @param[out] body Room for a body, or null to drop it.
/// @param[in,out] body_len How much room; on return, how much was written or needed.
/// @returns `LOWLAT_OK`, `LOWLAT_TIMEOUT`, `LOWLAT_ERR_TOO_SMALL`, or
/// `LOWLAT_ERR_INVALID_ARGUMENT`.
///
/// @attention `cl` came from `lowlat_client_create`; `out` points to one
/// `lowlat_event`; `body` is null or points to `*body_len` writable bytes.
lowlat_status lowlat_client_poll_events(lowlat_client *cl,
                                        uint32_t timeout_ms,
                                        lowlat_event *out,
                                        void *body,
                                        uint32_t *body_len) LOWLAT_NOEXCEPT;
#endif

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus
