/* Drives the built shared object the way an application does.
 *
 * Compiled twice, once as C and once as C++, both with warnings as errors:
 * a header that only works in the language its author used is a header half
 * the callers cannot use.
 *
 * It opens the shared object by path rather than linking it, because the
 * question is whether the *shipped* object still contains a panic. Linking the
 * same code into this program would answer for this program's build instead.
 */

/* Strict ISO C hides the monotonic clock, and the gate is worth more strict
 * than the timing is worth loose. */
#define _POSIX_C_SOURCE 200809L

#include "lowlat.h"

#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <time.h>

/* dlsym returns an object pointer and we need a function pointer. Casting
 * between the two is a constraint violation in C and needs a reinterpret in
 * C++; copying the bytes is neither, and is the one spelling that compiles
 * clean in both languages. */
#define RESOLVE(fn, lib, name)                                                 \
    do {                                                                       \
        void *raw = dlsym((lib), (name));                                      \
        if (raw == NULL) {                                                     \
            fprintf(stderr, "harness: %s is not exported\n", (name));          \
            return 2;                                                          \
        }                                                                      \
        memcpy(&(fn), &raw, sizeof raw);                                       \
    } while (0)

/* Counts the lines the library hands over, and proves each is a usable C
 * string with the caller's own pointer alongside it. */
static void logged(uint32_t level, const char *message, void *opaque)
{
    (void) level;
    if (message == NULL || opaque == NULL || message[0] == '\0') {
        return;
    }
    (void) strlen(message);   /* runs off the end if it is not terminated */
    *(unsigned *) opaque += 1;
}

int main(int argc, char **argv)
{
    unsigned log_lines = 0;
    if (argc < 2) {
        fprintf(stderr, "usage: harness <path to shared object>\n");
        return 2;
    }

    void *lib = dlopen(argv[1], RTLD_NOW);
    if (lib == NULL) {
        fprintf(stderr, "harness: dlopen failed: %s\n", dlerror());
        return 2;
    }

    uint32_t (*abi_version)(void);
    const char *(*status_string)(lowlat_status);
    lowlat_status (*debug_panic)(lowlat *);
    lowlat_status (*create)(const lowlat_create_info *, lowlat **);
    void (*destroy)(lowlat *);
    lowlat_status (*poll_events)(lowlat *, uint32_t, lowlat_event *, void *, uint32_t *);
    lowlat_status (*host_start)(lowlat *, const lowlat_host_config *);
    lowlat_status (*host_stop)(lowlat *);
    lowlat_status (*set_video)(lowlat *, const lowlat_host_video_config *);
    lowlat_status (*get_video)(lowlat *, lowlat_host_video_config *);
    lowlat_status (*set_audio)(lowlat *, const lowlat_host_audio_config *);
    lowlat_status (*get_audio)(lowlat *, lowlat_host_audio_config *);
    lowlat_status (*get_audio_outputs)(lowlat_audio_output *, uint32_t *);
    lowlat_status (*new_attempt)(lowlat *, const lowlat_attempt_info *);
    void (*add_candidate)(lowlat *, const char *, const lowlat_candidate *);
    lowlat_status (*begin_p2p)(lowlat *, const char *, uint16_t, lowlat_credentials *);
    void (*end_connection)(lowlat *, const char *);
    lowlat_status (*get_guests)(lowlat *, lowlat_guest *, uint32_t *);
    lowlat_status (*send_user_data)(lowlat *, uint32_t, uint32_t, const void *, uint32_t);
    lowlat_status (*send_roster)(lowlat *, const void *, uint32_t, uint32_t *);
    lowlat_status (*get_metrics)(lowlat *, uint32_t, lowlat_metrics *);
    lowlat_status (*set_permissions)(lowlat *, uint32_t, const lowlat_permissions *);
    lowlat_status (*kick_guest)(lowlat *, uint32_t, int32_t);
    lowlat_status (*can_host)(void);
    lowlat_status (*get_outputs)(lowlat_output *, uint32_t *);
    lowlat_status (*get_status)(lowlat *, lowlat_host_status *);
    lowlat_status (*set_log_callback)(void (*)(uint32_t, const char *, void *), void *);

    RESOLVE(abi_version, lib, "lowlat_abi_version");
    RESOLVE(status_string, lib, "lowlat_status_string");
    RESOLVE(debug_panic, lib, "lowlat_debug_panic");
    RESOLVE(create, lib, "lowlat_create");
    RESOLVE(destroy, lib, "lowlat_destroy");
    RESOLVE(poll_events, lib, "lowlat_host_poll_events");
    RESOLVE(host_start, lib, "lowlat_host_start");
    RESOLVE(host_stop, lib, "lowlat_host_stop");
    RESOLVE(set_video, lib, "lowlat_host_set_video_config");
    RESOLVE(get_video, lib, "lowlat_host_get_video_config");
    RESOLVE(new_attempt, lib, "lowlat_host_new_attempt");
    RESOLVE(add_candidate, lib, "lowlat_host_add_candidate");
    RESOLVE(begin_p2p, lib, "lowlat_host_begin_p2p");
    RESOLVE(end_connection, lib, "lowlat_host_end_connection");
    RESOLVE(get_guests, lib, "lowlat_host_get_guests");
    RESOLVE(send_user_data, lib, "lowlat_host_send_user_data");
    RESOLVE(send_roster, lib, "lowlat_host_send_roster");
    RESOLVE(get_metrics, lib, "lowlat_host_get_metrics");
    RESOLVE(set_permissions, lib, "lowlat_host_set_permissions");
    RESOLVE(kick_guest, lib, "lowlat_host_kick_guest");
    RESOLVE(can_host, lib, "lowlat_can_host");
    RESOLVE(get_outputs, lib, "lowlat_get_outputs");
    RESOLVE(set_audio, lib, "lowlat_host_set_audio_config");
    RESOLVE(get_audio, lib, "lowlat_host_get_audio_config");
    RESOLVE(get_audio_outputs, lib, "lowlat_get_audio_outputs");
    RESOLVE(get_status, lib, "lowlat_host_get_status");
    RESOLVE(set_log_callback, lib, "lowlat_set_log_callback");

    uint32_t version = abi_version();
    if ((version >> 16) != LOWLAT_ABI_MAJOR || (version & 0xffff) != LOWLAT_ABI_MINOR) {
        fprintf(stderr, "harness: the object reports version %u.%u, the header says %u.%u\n",
                version >> 16, version & 0xffff,
                (unsigned) LOWLAT_ABI_MAJOR, (unsigned) LOWLAT_ABI_MINOR);
        return 1;
    }

    /* Log lines reach the application, terminated, with its own pointer handed
     * back. This is the single place the library calls out. */
    if (set_log_callback(logged, &log_lines) != LOWLAT_OK) {
        fprintf(stderr, "harness: a log callback could not be registered\n");
        return 1;
    }

    /* Both of these answer before anything is created, which is the point of
     * them: an application presents a choice, or explains why it cannot. */
    lowlat_status host_able = can_host();
    if (host_able != LOWLAT_OK && host_able != LOWLAT_ERR_NO_DISPLAY
        && host_able != LOWLAT_ERR_DISPLAY_UNREACHABLE) {
        fprintf(stderr, "harness: the pre-flight answered %d (%s)\n",
                (int) host_able, status_string(host_able));
        return 1;
    }
    uint32_t outputs = 0;
    if (get_outputs(NULL, &outputs) != LOWLAT_OK) {
        fprintf(stderr, "harness: the outputs could not be counted\n");
        return 1;
    }
    if (outputs > 0) {
        lowlat_output listed[8];
        uint32_t room = outputs < 8 ? outputs : 8;
        lowlat_status got = get_outputs(listed, &room);
        if ((got != LOWLAT_OK && got != LOWLAT_ERR_TOO_SMALL) || listed[0].id[0] == '\0') {
            fprintf(stderr, "harness: an output was listed with no identity\n");
            return 1;
        }
    }

    lowlat_create_info info;
    info.size = (uint32_t) sizeof info;
    lowlat *ll = NULL;
    if (create(&info, &ll) != LOWLAT_OK || ll == NULL) {
        fprintf(stderr, "harness: a handle could not be created\n");
        return 1;
    }

    /* A poll waits for the time it was given and then says nothing arrived,
     * which is not an error. Timed here because a poll that returns at once is
     * a busy loop in every application that uses it. */
    lowlat_event event;
    struct timespec before, after;
    clock_gettime(CLOCK_MONOTONIC, &before);
    lowlat_status polled = poll_events(ll, 60, &event, NULL, NULL);
    clock_gettime(CLOCK_MONOTONIC, &after);
    if (polled != LOWLAT_TIMEOUT) {
        fprintf(stderr, "harness: an empty poll returned %d, not %d\n",
                (int) polled, (int) LOWLAT_TIMEOUT);
        return 1;
    }
    double waited_ms = (double) (after.tv_sec - before.tv_sec) * 1000.0
                     + (double) (after.tv_nsec - before.tv_nsec) / 1000000.0;
    if (waited_ms < 50.0) {
        fprintf(stderr, "harness: a 60 ms poll returned after %.1f ms\n", waited_ms);
        return 1;
    }

    /* Hosting, configured the way an application would: no resolution to ask
     * for, an output left empty so the host picks, and a frame rate that is a
     * ceiling rather than a target. */
    lowlat_host_config cfg;
    memset(&cfg, 0, sizeof cfg);
    cfg.size = (uint32_t) sizeof cfg;
    cfg.base_port = 9100;
    cfg.max_guests = 4;
    cfg.codec = LOWLAT_CODEC_H264;
    cfg.encoder = LOWLAT_ENCODER_FOLLOW_DISPLAY;
    cfg.cg_level = LOWLAT_CG_LEVEL_SENSITIVE;
    cfg.exclusive_hold_ms = 500;
    cfg.video.size = (uint32_t) sizeof cfg.video;
    cfg.video.fps = 60;
    cfg.video.bitrate_mbps = 10.0;
    cfg.video.min_bitrate_mbps = 1.0;
    cfg.video.full_fps = true;
    cfg.audio.size = (uint32_t) sizeof cfg.audio;
    cfg.audio.bitrate_kbps = 128;
    /* Off, because a harness that opened a sound device would be testing the
     * machine it runs on rather than this boundary. */
    cfg.audio.enabled = false;
    cfg.audio.allow_uncompressed = false;
    cfg.audio.mute_local = false;
    if (host_start(ll, &cfg) != LOWLAT_OK) {
        fprintf(stderr, "harness: hosting would not start\n");
        return 1;
    }
    if (host_start(ll, &cfg) != LOWLAT_ERR_ALREADY_STARTED) {
        fprintf(stderr, "harness: starting twice was not refused\n");
        return 1;
    }

    /* A value nothing defines must be refused, not read as a variant that does
     * not exist. */
    lowlat_host_config bad = cfg;
    bad.codec = 99;
    if (host_start(ll, &bad) != LOWLAT_ERR_ALREADY_STARTED) {
        fprintf(stderr, "harness: a running host accepted a second configuration\n");
        return 1;
    }
    if (host_stop(ll) != LOWLAT_OK) {
        fprintf(stderr, "harness: hosting would not stop\n");
        return 1;
    }
    if (host_start(ll, &bad) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a codec nothing defines was accepted\n");
        return 1;
    }

    /* The live half, changed while the host runs and without rebuilding the
     * session: what the boundary can set is what it reads back. */
    if (host_start(ll, &cfg) != LOWLAT_OK) {
        fprintf(stderr, "harness: hosting would not restart\n");
        return 1;
    }
    lowlat_host_video_config video = cfg.video;
    video.fps = 30;
    video.bitrate_mbps = 4.0;
    video.full_fps = false;
    if (set_video(ll, &video) != LOWLAT_OK) {
        fprintf(stderr, "harness: a live video change was refused\n");
        return 1;
    }
    lowlat_host_video_config back;
    memset(&back, 0, sizeof back);
    back.size = (uint32_t) sizeof back;
    if (get_video(ll, &back) != LOWLAT_OK) {
        fprintf(stderr, "harness: the live video settings could not be read back\n");
        return 1;
    }
    if (back.fps != 30 || back.bitrate_mbps != 4.0 || back.full_fps != false) {
        fprintf(stderr, "harness: read back fps=%u bitrate=%.1f full=%u after setting 30/4.0/0\n",
                back.fps, back.bitrate_mbps, (unsigned) back.full_fps);
        return 1;
    }
    /* A floor above the ceiling is refused rather than silently reordered. */
    video.min_bitrate_mbps = 100.0;
    if (set_video(ll, &video) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a floor above the ceiling was accepted\n");
        return 1;
    }

    /* Sound, every field of which is live. Read back through the host rather
     * than from the copy that was written, so a setting that reached nothing
     * shows up here. */
    lowlat_host_audio_config audio = cfg.audio;
    audio.bitrate_kbps = 96;
    audio.allow_uncompressed = true;
    if (set_audio(ll, &audio) != LOWLAT_OK) {
        fprintf(stderr, "harness: a live audio change was refused\n");
        return 1;
    }
    lowlat_host_audio_config audio_back;
    memset(&audio_back, 0, sizeof audio_back);
    audio_back.size = (uint32_t) sizeof audio_back;
    if (get_audio(ll, &audio_back) != LOWLAT_OK) {
        fprintf(stderr, "harness: the live audio settings could not be read back\n");
        return 1;
    }
    if (audio_back.bitrate_kbps != 96 || audio_back.allow_uncompressed != true) {
        fprintf(stderr, "harness: read back %u kbit/s uncompressed=%u after setting 96/1\n",
                audio_back.bitrate_kbps, (unsigned) audio_back.allow_uncompressed);
        return 1;
    }
    /* A rate nothing can serve is refused rather than clamped in silence. */
    audio.bitrate_kbps = 0;
    if (set_audio(ll, &audio) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a rate of zero was accepted\n");
        return 1;
    }
    audio.bitrate_kbps = LOWLAT_AUDIO_KBPS_MAX + 1;
    if (set_audio(ll, &audio) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a rate past the ceiling was accepted\n");
        return 1;
    }

    /* Enumerating sound outputs, which answers before a host is started and
     * without disturbing one that is. A machine with none answers zero. */
    uint32_t sound_outputs = 0;
    if (get_audio_outputs(NULL, &sound_outputs) != LOWLAT_OK) {
        fprintf(stderr, "harness: counting sound outputs failed\n");
        return 1;
    }
    if (sound_outputs > 0) {
        lowlat_audio_output found[8];
        uint32_t room = sound_outputs < 8 ? sound_outputs : 8;
        lowlat_status listed = get_audio_outputs(found, &room);
        if ((listed != LOWLAT_OK && listed != LOWLAT_ERR_TOO_SMALL) || found[0].id[0] == '\0') {
            fprintf(stderr, "harness: a sound output came back without an identity\n");
            return 1;
        }
        /* No room at all: told what it needs, and nothing written past the
         * end. */
        room = 0;
        if (get_audio_outputs(found, &room) != LOWLAT_ERR_TOO_SMALL || room != sound_outputs) {
            fprintf(stderr, "harness: a short buffer did not report the count needed\n");
            return 1;
        }
    }
    /* The signaling seam, in the order an application drives it. Everything
     * here arrived over a transport this library does not have: registering an
     * offer, trickling what the peer said it might be reachable at, approving,
     * and answering with credentials the application sends itself. */
    lowlat_attempt_info offer;
    memset(&offer, 0, sizeof offer);
    offer.size = (uint32_t) sizeof offer;
    snprintf(offer.attempt_id, sizeof offer.attempt_id, "%s", "3dea9cd3-3dc4a5c3");
    snprintf(offer.ufrag, sizeof offer.ufrag, "%s", "G+sZxQ==");
    snprintf(offer.pwd, sizeof offer.pwd, "%s", "Det3D+arYViymh6I2v7UaOnrsHieoTRE");
    offer.permissions.keyboard = true;
    offer.permissions.pointer = true;
    offer.permissions.gamepad = true;
    if (new_attempt(ll, &offer) != LOWLAT_OK) {
        fprintf(stderr, "harness: an offer could not be registered\n");
        return 1;
    }

    lowlat_candidate cand;
    memset(&cand, 0, sizeof cand);
    cand.size = (uint32_t) sizeof cand;
    cand.port = 41000;
    cand.sync = true;   /* a readiness marker, which carries no address */
    add_candidate(ll, offer.attempt_id, &cand);
    cand.sync = false;
    snprintf(cand.address, sizeof cand.address, "%s", "192.168.1.100");
    add_candidate(ll, offer.attempt_id, &cand);

    lowlat_credentials ours;
    memset(&ours, 0, sizeof ours);
    ours.size = (uint32_t) sizeof ours;
    if (begin_p2p(ll, offer.attempt_id, 0, &ours) != LOWLAT_OK) {
        fprintf(stderr, "harness: an attempt could not be approved\n");
        return 1;
    }
    /* The port that was bound, which is not necessarily the one configured:
     * the bind walks when a port is taken, and advertising the configured one
     * gives the peer an address that answers checks and never establishes. */
    if (ours.port == 0 || ours.ufrag[0] == '\0' || ours.fingerprint[0] == '\0') {
        fprintf(stderr, "harness: approval answered with nothing to send back\n");
        return 1;
    }
    if (strlen(ours.aes256) != 254) {
        fprintf(stderr, "harness: the media key is %zu characters, not 254\n",
                strlen(ours.aes256));
        return 1;
    }
    if (begin_p2p(ll, offer.attempt_id, 0, &ours) != LOWLAT_ERR_ALREADY_BEGUN) {
        fprintf(stderr, "harness: approving twice was not refused\n");
        return 1;
    }
    /* The roster, in the two calls an application makes: how many, then who.
     * Nothing is allocated on the caller's behalf, so there is nothing to
     * free. */
    uint32_t guests = 0;
    if (get_guests(ll, NULL, &guests) != LOWLAT_OK || guests != 1) {
        fprintf(stderr, "harness: an approved guest is not on the roster (%u)\n", guests);
        return 1;
    }
    lowlat_guest roster[4];
    guests = 4;
    if (get_guests(ll, roster, &guests) != LOWLAT_OK || guests != 1) {
        fprintf(stderr, "harness: the roster could not be read\n");
        return 1;
    }
    if (!roster[0].permissions.keyboard || roster[0].number == 0) {
        fprintf(stderr, "harness: the roster lost what signaling said about the guest\n");
        return 1;
    }

    /* An application message, uninterpreted in both directions. */
    const char *hello = "hello";
    if (send_user_data(ll, roster[0].number, 9, hello, 5) != LOWLAT_OK) {
        fprintf(stderr, "harness: a message to a seated guest was refused\n");
        return 1;
    }
    if (send_user_data(ll, 4242, 9, hello, 5) != LOWLAT_ERR_UNKNOWN_GUEST) {
        fprintf(stderr, "harness: a message to nobody was accepted\n");
        return 1;
    }

    /* What the host is doing, which is not what it was asked to do: the
     * picture's size is the display's answer and the guest count is the
     * room's. */
    lowlat_host_status state;
    memset(&state, 0, sizeof state);
    state.size = (uint32_t) sizeof state;
    if (get_status(ll, &state) != LOWLAT_OK || !state.running || state.guests != 1) {
        fprintf(stderr, "harness: the host reports running=%u guests=%u\n",
                (unsigned) state.running, state.guests);
        return 1;
    }

    /* The guest carries the attempt it was registered under, which is the link
     * between the two halves of the seam: everything before a guest is seated
     * is addressed by attempt and everything after by number. */
    if (strcmp(roster[0].attempt, offer.attempt_id) != 0) {
        fprintf(stderr, "harness: guest %u reports attempt %s, not %s\n",
                roster[0].number, roster[0].attempt, offer.attempt_id);
        return 1;
    }

    /* Metrics behind their own call, because a guest is an array element and
     * cannot carry a size of its own. */
    lowlat_metrics metrics;
    memset(&metrics, 0, sizeof metrics);
    metrics.size = (uint32_t) sizeof metrics;
    if (get_metrics(ll, roster[0].number, &metrics) != LOWLAT_OK) {
        fprintf(stderr, "harness: metrics could not be read\n");
        return 1;
    }
    /* Zero is how "never" is written, and a guest that has sent nothing has
     * never touched anything. */
    if (metrics.keyboard_ms != 0 || metrics.pointer_ms != 0) {
        fprintf(stderr, "harness: a guest that sent nothing reported input times\n");
        return 1;
    }
    if (get_metrics(ll, 4242, &metrics) != LOWLAT_ERR_UNKNOWN_GUEST) {
        fprintf(stderr, "harness: metrics answered for a guest that is not there\n");
        return 1;
    }

    /* The roster, which is what tells a guest what it is: a peer has no way to
     * ask for one and finds itself in the list by number. */
    const char *who = "[{\"id\":1}]";
    uint32_t reached = 0;
    if (send_roster(ll, who, (uint32_t) strlen(who), &reached) != LOWLAT_OK || reached != 1) {
        fprintf(stderr, "harness: the roster reached %u guest(s)\n", reached);
        return 1;
    }

    /* What a guest may drive, changed while it is connected. There is no
     * separate call to turn its input off: that is this one with every flag
     * cleared. */
    lowlat_permissions perms;
    perms.keyboard = false;
    perms.pointer = true;
    perms.gamepad = false;
    perms.reserved = 0;
    if (set_permissions(ll, roster[0].number, &perms) != LOWLAT_OK) {
        fprintf(stderr, "harness: permissions could not be changed\n");
        return 1;
    }
    guests = 4;
    if (get_guests(ll, roster, &guests) != LOWLAT_OK || roster[0].permissions.keyboard) {
        fprintf(stderr, "harness: the roster did not follow the change\n");
        return 1;
    }

    /* **Zero is not a reason.** A peer carries on through a status of zero, so
     * a guest kicked with one is told nothing and stays exactly where it was. */
    if (kick_guest(ll, roster[0].number, 0) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a status a peer ignores was accepted as a reason\n");
        return 1;
    }
    if (kick_guest(ll, roster[0].number, -15000) != LOWLAT_OK) {
        fprintf(stderr, "harness: a guest could not be kicked\n");
        return 1;
    }

    end_connection(ll, offer.attempt_id);

    if (host_stop(ll) != LOWLAT_OK) {
        fprintf(stderr, "harness: hosting would not stop again\n");
        return 1;
    }
    /* And with nothing running there is nothing for it to apply to. */
    if (set_video(ll, &cfg.video) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a video change was accepted with no host running\n");
        return 1;
    }

    /* An argument that cannot be written to is refused rather than used. */
    if (poll_events(ll, 0, NULL, NULL, NULL) != LOWLAT_ERR_INVALID_ARGUMENT) {
        fprintf(stderr, "harness: a poll with nowhere to put the event was not refused\n");
        return 1;
    }

    /* The point of the whole program. A panic inside the library must arrive
     * here as a value; if containment is off, this call unwinds through a
     * frame compiled by another language and takes the process with it. */
    lowlat_status contained = debug_panic(ll);
    if (contained != LOWLAT_ERR_INTERNAL) {
        fprintf(stderr, "harness: a deliberate panic returned %d, not %d\n",
                (int) contained, (int) LOWLAT_ERR_INTERNAL);
        return 1;
    }

    /* And what follows one is refused, on a handle that can still be
     * destroyed. */
    if (poll_events(ll, 0, &event, NULL, NULL) != LOWLAT_ERR_POISONED) {
        fprintf(stderr, "harness: a call after a contained panic was not refused\n");
        return 1;
    }

    /* And it has to describe itself, because a status an application cannot
     * name is one it will report as a number nobody can look up. */
    const char *described = status_string(contained);
    if (described == NULL || described[0] == '\0') {
        fprintf(stderr, "harness: the contained status describes itself as nothing\n");
        return 1;
    }

    destroy(ll);

    if (log_lines == 0) {
        fprintf(stderr, "harness: a whole session produced no log lines\n");
        return 1;
    }

    printf("harness: version %u.%u, a panic returned %d (%s)\n",
           version >> 16, version & 0xffff, (int) contained, described);
    return 0;
}
