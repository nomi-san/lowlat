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

int main(int argc, char **argv)
{
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
    lowlat_status (*new_attempt)(lowlat *, const lowlat_attempt_info *);
    void (*add_candidate)(lowlat *, const char *, const lowlat_candidate *);
    lowlat_status (*begin_p2p)(lowlat *, const char *, lowlat_credentials *);
    void (*end_connection)(lowlat *, const char *);
    lowlat_status (*get_guests)(lowlat *, lowlat_guest *, uint32_t *);
    lowlat_status (*send_user_data)(lowlat *, uint32_t, uint32_t, const void *, uint32_t);
    lowlat_status (*set_permissions)(lowlat *, uint32_t, const lowlat_permissions *);
    lowlat_status (*kick_guest)(lowlat *, uint32_t, int32_t);
    lowlat_status (*can_host)(void);
    lowlat_status (*get_outputs)(lowlat_output *, uint32_t *);

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
    RESOLVE(set_permissions, lib, "lowlat_host_set_permissions");
    RESOLVE(kick_guest, lib, "lowlat_host_kick_guest");
    RESOLVE(can_host, lib, "lowlat_can_host");
    RESOLVE(get_outputs, lib, "lowlat_get_outputs");

    uint32_t version = abi_version();
    if ((version >> 16) != LOWLAT_ABI_MAJOR || (version & 0xffff) != LOWLAT_ABI_MINOR) {
        fprintf(stderr, "harness: the object reports version %u.%u, the header says %u.%u\n",
                version >> 16, version & 0xffff,
                (unsigned) LOWLAT_ABI_MAJOR, (unsigned) LOWLAT_ABI_MINOR);
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
    if (begin_p2p(ll, offer.attempt_id, &ours) != LOWLAT_OK) {
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
    if (begin_p2p(ll, offer.attempt_id, &ours) != LOWLAT_ERR_ALREADY_BEGUN) {
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

    printf("harness: version %u.%u, a panic returned %d (%s)\n",
           version >> 16, version & 0xffff, (int) contained, described);
    return 0;
}
