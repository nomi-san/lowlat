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

    RESOLVE(abi_version, lib, "lowlat_abi_version");
    RESOLVE(status_string, lib, "lowlat_status_string");
    RESOLVE(debug_panic, lib, "lowlat_debug_panic");
    RESOLVE(create, lib, "lowlat_create");
    RESOLVE(destroy, lib, "lowlat_destroy");
    RESOLVE(poll_events, lib, "lowlat_host_poll_events");
    RESOLVE(host_start, lib, "lowlat_host_start");
    RESOLVE(host_stop, lib, "lowlat_host_stop");

    uint32_t version = abi_version();
    if ((version >> 16) != LOWLAT_ABI_MAJOR || (version & 0xffff) != LOWLAT_ABI_MINOR) {
        fprintf(stderr, "harness: the object reports version %u.%u, the header says %u.%u\n",
                version >> 16, version & 0xffff,
                (unsigned) LOWLAT_ABI_MAJOR, (unsigned) LOWLAT_ABI_MINOR);
        return 1;
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
    cfg.rotation = LOWLAT_ROTATION_NONE;
    cfg.fps = 60;
    cfg.bitrate_mbps = 10.0;
    cfg.min_bitrate_mbps = 1.0;
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
