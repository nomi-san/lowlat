/* lowlat - the public C ABI.
 *
 * Generated from the Rust definitions. Do not edit; see crates/host/cbindgen.toml.
 */

#pragma once

#include <stdint.h>

// The major version, raised only when something already published changes.
#define LOWLAT_ABI_MAJOR 0

// The minor version, raised when surface is appended.
#define LOWLAT_ABI_MINOR 1

// The longest attempt identifier carried across this boundary.
//
// **A fixed array rather than a pointer**, because nothing crosses here that
// the application has to free (docs/06-api.md 10). An identifier longer than
// this is the application's own, so it is truncated on the way out rather
// than refused: the event still says what happened, and the application
// already holds the identifier it made up.
#define LOWLAT_ATTEMPT_MAX 128

// The longest textual address, which is what an address for a peer's
// signaling to forward has to be anyway.
#define LOWLAT_ADDRESS_MAX 46

// The longest output identity carried across this boundary.
#define LOWLAT_OUTPUT_MAX 64

// How many reflexive servers a host may be given, and how long each may be.
//
// **A fixed array rather than a pointer and a count**, so the structure stays
// one blittable block with nothing in it to free. Four is already more than
// any host here has ever been configured with.
#define LOWLAT_SERVERS_MAX 4

// The longest textual `host:port` for one of them.
#define LOWLAT_SERVER_MAX 64

// The most guests a host may advertise, which is what the ring memory per
// guest is sized against.
#define LOWLAT_GUESTS_MAX 16

// A status code.
//
// **An enumeration for the names and a plain integer wherever one is
// accepted.** Grouping the codes under a type is what tells a reader that
// `LOWLAT_TIMEOUT` is a status and `LOWLAT_ATTEMPT_MAX` is a size; taking one
// back by value as this type would be something else entirely, because
// reading a discriminant nothing defined is undefined behaviour and an
// application is free to hand back any integer it has.
//
// Zero succeeds, positive is a non-fatal condition, negative is an error, and
// the error space is partitioned by subsystem so that a number says where it
// came from without a lookup:
//
// ```text
//   -1 to -99      the boundary itself: arguments, state, contained faults
//   -100 to -199   signaling and admission
//   -200 to -299   capture
//   -300 to -399   encode
//   -400 to -499   transport
// ```
//
// A value is assigned once and never reused, including for a condition that
// is removed.
enum lowlat_status
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : int32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    // The call succeeded.
    LOWLAT_OK = 0,
    // No event arrived within the timeout. Not an error.
    LOWLAT_TIMEOUT = 1,
    // A fault was contained at the boundary. The handle no longer runs.
    LOWLAT_ERR_INTERNAL = -1,
    // An argument was missing, out of range, or contradicted another.
    LOWLAT_ERR_INVALID_ARGUMENT = -2,
    // The buffer was too small. What it would have taken has been written
    // back, and nothing has been consumed.
    LOWLAT_ERR_TOO_SMALL = -3,
    // A previous call was contained at the boundary, so this handle is no
    // longer trusted to describe its own state. Only destroying it still
    // works.
    LOWLAT_ERR_POISONED = -4,
    // This handle is already hosting. Stopping first is the way to start
    // again with a different configuration.
    LOWLAT_ERR_ALREADY_STARTED = -5,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_status lowlat_status;
#else
typedef int32_t lowlat_status;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// Which member of an event is the valid one.
enum lowlat_event_type
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    // A local candidate, to be sent to the peer as it is found.
    LOWLAT_EVENT_CANDIDATE = 1,
    // Send the peer a candidate marked ready, once.
    LOWLAT_EVENT_READY = 2,
    // Connectivity completed and media can flow.
    LOWLAT_EVENT_ESTABLISHED = 3,
    // The attempt is over, with a reason.
    LOWLAT_EVENT_ENDED = 4,
    // A guest sent its application a message.
    LOWLAT_EVENT_USER_DATA = 5,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_event_type lowlat_event_type;
#else
typedef uint32_t lowlat_event_type;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// Why an attempt finished.
enum lowlat_outcome
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    // Negotiated, and no path was found.
    LOWLAT_OUTCOME_CONNECTIVITY_FAILED = 1,
    // The peer stopped answering.
    LOWLAT_OUTCOME_PEER_GONE = 2,
    // Nothing sent has been acknowledged for the delivery deadline, while
    // something was outstanding the whole time.
    LOWLAT_OUTCOME_UNDELIVERABLE = 3,
    // The peer said it was leaving.
    LOWLAT_OUTCOME_PEER_LEFT = 4,
    // Connected, then never said what it could decode.
    LOWLAT_OUTCOME_NEVER_DECLARED = 5,
    // The socket could not be driven any further.
    LOWLAT_OUTCOME_TRANSPORT_FAILED = 6,
    // The control stream could not be read any further.
    LOWLAT_OUTCOME_CONTROL_STALLED = 7,
    // The host ended it, and `reason` carries what the peer was told.
    LOWLAT_OUTCOME_KICKED = 8,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_outcome lowlat_outcome;
#else
typedef uint32_t lowlat_outcome;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// Which codec the stream is encoded with.
//
// **Named by an enumeration and carried as an integer**, for the reason
// [`lowlat_status`] is: the application writes this field, so the value
// arriving is whatever it wrote.
enum lowlat_codec
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    LOWLAT_CODEC_H264 = 1,
    LOWLAT_CODEC_HEVC = 2,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_codec lowlat_codec;
#else
typedef uint32_t lowlat_codec;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// Which encoder to build.
enum lowlat_encoder
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    // **The default, and the right one.** A conversion target is allocated on
    // the device the display is on and an encoder belonging to another cannot
    // take it, so the encoder is a consequence of where the display is rather
    // than a preference. Choosing one is for forcing a particular encoder on a
    // machine where either would do.
    LOWLAT_ENCODER_FOLLOW_DISPLAY = 0,
    LOWLAT_ENCODER_OPEN = 1,
    LOWLAT_ENCODER_VENDOR = 2,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_encoder lowlat_encoder;
#else
typedef uint32_t lowlat_encoder;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// How the display this stream shows is oriented.
//
// **The coded picture never rotates.** This travels to the peer, which is what
// presents the picture and what maps pointer coordinates against it.
enum lowlat_rotation
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    LOWLAT_ROTATION_NONE = 1,
    LOWLAT_ROTATION_90 = 2,
    LOWLAT_ROTATION_180 = 3,
    LOWLAT_ROTATION_270 = 4,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_rotation lowlat_rotation;
#else
typedef uint32_t lowlat_rotation;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// Which congestion control level a session runs at.
//
// **Zero is the most aggressive, not "off".** Its threshold declares
// congestion on any stale fragment once the send window passes its floor, and
// it exists only for compatibility with an older scheme. Sensitive is the
// default and the one to leave alone.
enum lowlat_cg_level
#if defined(__cplusplus) || __STDC_VERSION__ >= 202311L
  : uint32_t
#endif // defined(__cplusplus) || __STDC_VERSION__ >= 202311L
 {
    LOWLAT_CG_LEVEL_LEGACY = 0,
    LOWLAT_CG_LEVEL_SENSITIVE = 1,
    LOWLAT_CG_LEVEL_RELAXED = 2,
};
#ifndef __cplusplus
#if __STDC_VERSION__ >= 202311L
typedef enum lowlat_cg_level lowlat_cg_level;
#else
typedef uint32_t lowlat_cg_level;
#endif // __STDC_VERSION__ >= 202311L
#endif // __cplusplus

// One host session, as the application holds it.
//
// Opaque: the application holds a pointer it cannot look inside, so what is
// in here changes freely.
typedef struct lowlat lowlat;

// What a handle is created with.
//
// **The caller sets `size`.** It is read rather than assumed, so this can grow
// without breaking an application compiled against an older header
// (docs/06-api.md 1).
typedef struct lowlat_create_info {
    uint32_t size;
} lowlat_create_info;

// The video settings that can change while a host is running.
//
// **Split out because the split is real.** Everything here is applied without
// rebuilding anything the session rests on: a bitrate re-bases the budget and
// reaches the encoder through the reconfigure the rate loop already performs,
// and a frame rate changes the pacing from the next frame. What is not here --
// the codec, the encoder, the guest limit, the ports -- is settled when
// hosting starts, because changing it means building the pipeline again.
//
// **There is no resolution and no rotation.** The display decides its own size
// and orientation and this host follows; asking it to be something else is a
// request to whoever owns the display, which is not this library
// ([impl-plan](../../../docs/impl-plan.md), *Output selection*).
typedef struct lowlat_host_video_config {
    // Set by the caller to `sizeof(lowlat_host_video_config)`.
    uint32_t size;
    // **A ceiling, not a target.** Capture runs at the display's own rate and
    // this is the most that is encoded from it.
    uint32_t fps;
    // What the operator asked for, before it is divided among guests.
    double bitrate_mbps;
    // The floor congestion control may not descend below. Lowered with the
    // ceiling when it would otherwise sit above it.
    double min_bitrate_mbps;
    // Emit at `fps` even when the picture has not changed.
    //
    // **Clearing it is a permission, not an instruction.** There is no damage
    // signal here, so nothing yet skips a repeated picture; a host that keeps
    // sending costs bitrate rather than being wrong.
    uint8_t full_fps;
    uint8_t reserved[3];
    // Which output to capture, by an identity from the enumeration. **Empty
    // means whichever this host would pick on its own**, which is the output
    // at the desktop's corner and then whatever is lit.
    char output[LOWLAT_OUTPUT_MAX];
} lowlat_host_video_config;

// How a host is configured.
//
// **There is no resolution here.** The display decides the picture's size, the
// encoder follows it, and the application is told what it got rather than
// asking for it; `fps` is a cap over whatever the display runs at, not a
// target. A host that creates its own display chooses that display's size when
// it creates it, which is a different question and not this field's.
typedef struct lowlat_host_config {
    // Set by the caller to `sizeof(lowlat_host_config)`.
    uint32_t size;
    // The base a guest's port bind walks from.
    uint16_t base_port;
    uint16_t reserved;
    // Advertised capacity. Above [`LOWLAT_GUESTS_MAX`] is refused rather than
    // quietly reduced.
    uint32_t max_guests;
    // One of [`lowlat_codec`]. Settled when hosting starts: one encode serves
    // every seat and a session has one video configuration.
    uint32_t codec;
    // One of [`lowlat_encoder`].
    uint32_t encoder;
    // One of [`lowlat_cg_level`].
    uint32_t cg_level;
    // How long a guest keeps the pointer after its last movement, when
    // `exclusive_pointer` is set. Clamped rather than refused: this is a
    // comfort setting and the nearest usable value beats refusing to start.
    uint32_t exclusive_hold_ms;
    // Whether one guest at a time may drive the pointer. Off means everybody
    // drives it, which is a configuration rather than a fault.
    uint8_t exclusive_pointer;
    uint8_t reserved2[3];
    // How many of `servers` are set.
    uint32_t server_count;
    // Reflexive servers, consulted for this host's own mapped address, each
    // `host:port`.
    char servers[LOWLAT_SERVERS_MAX][LOWLAT_SERVER_MAX];
    // The half of this that can also be set while the host runs.
    struct lowlat_host_video_config video;
} lowlat_host_config;

// A local candidate for the application to forward.
typedef struct lowlat_candidate_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
    char address[LOWLAT_ADDRESS_MAX];
    uint16_t port;
    // Non-zero if a reflexive server reported this one.
    uint8_t from_stun;
    uint8_t reserved;
} lowlat_candidate_event;

// Tell the peer this host is ready to be checked.
typedef struct lowlat_ready_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
} lowlat_ready_event;

// A path was found and media is flowing.
typedef struct lowlat_established_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
    char address[LOWLAT_ADDRESS_MAX];
    uint16_t port;
    uint8_t reserved[2];
} lowlat_established_event;

// The attempt is over.
typedef struct lowlat_ended_event {
    char attempt[LOWLAT_ATTEMPT_MAX];
    lowlat_outcome outcome;
    // What the peer was told, when the outcome is that the host ended it.
    // Zero otherwise, and zero is not a status a peer stops on.
    int32_t reason;
} lowlat_ended_event;

// An application message from a guest.
typedef struct lowlat_user_data_event {
    uint32_t guest;
    // The sub-identifier, which means whatever the application and its
    // clients agreed it means. Nothing here reads it.
    uint32_t id;
    // How long the body is. **Not how much was written**: a caller that
    // offered no buffer is still told what it chose not to receive.
    uint32_t body_len;
} lowlat_user_data_event;

// Whichever event this is.
//
// A union cannot describe itself, and the tag beside it is what says which
// member to read.
typedef union lowlat_event_body {
    struct lowlat_candidate_event candidate;
    struct lowlat_ready_event ready;
    struct lowlat_established_event established;
    struct lowlat_ended_event ended;
    struct lowlat_user_data_event user_data;
} lowlat_event_body;

// One event.
//
// **The tag is first** so an application that does not recognise a type can
// skip it without knowing anything about the rest, which is what makes adding
// a type additive.
typedef struct lowlat_event {
    lowlat_event_type kind;
    // How many events were dropped since the previous delivery.
    //
    // **Carried on the next event rather than reported at the time**, which
    // is the only place it can be: the drop happened because nobody was
    // polling.
    uint32_t dropped;
    union lowlat_event_body body;
} lowlat_event;

#ifdef __cplusplus
extern "C" {
#endif // __cplusplus

// Major and minor, packed.
//
// **The one function whose signature can never change**, because it is what a
// loader calls to decide whether it may call anything else.
uint32_t lowlat_abi_version(void);

// Describe a status.
//
// **It takes a plain integer rather than the enumeration**, so that a value
// from anywhere can be described -- including one this version of the library
// does not define, which is exactly the case an application reaches for this
// in. Passing a status to it is an ordinary widening conversion.
//
// The pointer is to storage that outlives the library, so it is never freed
// and never copied out of.
const char *lowlat_status_string(int32_t status);

// Create a handle.
//
// # Safety
//
// `out` must point to storage for one pointer. `info` may be null, which
// takes every default.
lowlat_status lowlat_create(const struct lowlat_create_info *info, struct lowlat **out);

// Destroy a handle.
//
// **Works on a poisoned handle**, which is the point of poisoning: everything
// else is refused and this still releases what was taken.
//
// # Safety
//
// `ll` came from [`lowlat_create`] and is not used again. A null pointer is
// accepted and does nothing.
void lowlat_destroy(struct lowlat *ll);

// Start hosting.
//
// Guests are admitted through the signaling seam, which is the application's
// own; this starts what serves them once they arrive.
//
// # Safety
//
// `ll` came from [`lowlat_create`], and `cfg` points to one
// [`lowlat_host_config`] whose `size` says how much of it is set.
lowlat_status lowlat_host_start(struct lowlat *ll,
                                const struct lowlat_host_config *cfg);

// Change the video settings while the host runs.
//
// **Everything in this structure is applied without rebuilding the session.**
// The bitrate re-bases the budget and reaches the encoder through the
// reconfigure the rate loop already does, so it costs no keyframe and no
// interruption; the frame rate changes the pacing from the next frame. The
// output is the exception in cost rather than in kind: a different picture
// cannot be absorbed into a stream built for another one, so it rebuilds
// around the new source and costs one coded refresh, keeping every guest on
// its seat and its channel.
//
// Refused with [`LOWLAT_ERR_INVALID_ARGUMENT`] when the host is not running,
// because there is nothing yet for the values to apply to and accepting them
// silently would report settings that never took.
//
// # Safety
//
// `ll` came from [`lowlat_create`], and `cfg` points to one
// [`lowlat_host_video_config`] whose `size` says how much of it is set.
lowlat_status lowlat_host_set_video_config(struct lowlat *ll,
                                           const struct lowlat_host_video_config *cfg);

// What the host is running at now.
//
// **Read back rather than remembered.** What a stream is doing is the
// stream's answer, and an application that kept its own copy would be
// describing settings another guest may have changed underneath it.
//
// # Safety
//
// `ll` came from [`lowlat_create`], and `out` points to one
// [`lowlat_host_video_config`] whose `size` says how much of it is set.
lowlat_status lowlat_host_get_video_config(struct lowlat *ll,
                                           struct lowlat_host_video_config *out);

// Stop hosting, disconnecting every guest and joining every thread.
//
// **Not the same as destroying the handle.** A host may be stopped and started
// again on the same handle, and events raised before it stopped are still
// waiting to be polled.
//
// **A peer is not yet told why.** Guest loops are stopped and joined, and the
// far side learns by its own liveness deadline rather than from a message, so
// stopping costs a peer the wait rather than being immediate to it. There is
// no reason parameter here because there is nothing yet that could carry one.
//
// # Safety
//
// `ll` came from [`lowlat_create`].
lowlat_status lowlat_host_stop(struct lowlat *ll);

// Take one event, waiting up to `timeout_ms` for one to arrive.
//
// Answers [`LOWLAT_TIMEOUT`] when nothing arrived, which is not an error. A
// `timeout_ms` of zero polls without waiting.
//
// `body` receives an application message's body and may be null, which means
// the application does not want bodies: one that arrives is delivered without
// it, and the event still says how long it was. When `body` is not null,
// `body_len` carries its capacity in and the bytes written out.
//
// **A body that does not fit consumes nothing.** [`LOWLAT_ERR_TOO_SMALL`] is
// answered, `body_len` is set to what the body needs, and the same event is
// delivered by the next call with room for it.
//
// # Safety
//
// `out` must point to one [`lowlat_event`]. `body`, when not null, must point
// to at least `*body_len` bytes, and `body_len` must then be readable and
// writable.
lowlat_status lowlat_host_poll_events(struct lowlat *ll,
                                      uint32_t timeout_ms,
                                      struct lowlat_event *out,
                                      void *body,
                                      uint32_t *body_len);

// Panic on purpose, and prove the boundary contains it.
//
// **Exported by the shipped library rather than hidden behind a build
// option**, because what has to be tested is that *this* object still
// unwinds. Building it to abort on panic silently disables containment
// everywhere, and the same code linked into a test binary answers for the
// test's build rather than for this one.
// **It takes the handle** so that what follows a contained panic is testable
// too: the handle is poisoned, every later call on it is refused, and
// destroying it still works.
//
// # Safety
//
// `ll` came from [`lowlat_create`].
lowlat_status lowlat_debug_panic(struct lowlat *ll);

#ifdef __cplusplus
}  // extern "C"
#endif  // __cplusplus
