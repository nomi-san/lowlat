/* Drives the static library the way an application that links it does.
 *
 * Linked rather than opened by path, with nothing on the link line but the
 * library: the system libraries it calls into are named inside it, and a name
 * missing there is an unresolved symbol here. Only the client half, which is
 * what the library carries where it is built as a static library; its
 * deliberate panic is still held at the boundary, which is the one property a
 * different form of linking could lose. Compiled as C++, it throws and catches
 * exceptions of its own on either side of that panic: both go through the
 * system's one unwinder.
 */

#include "lowlat.h"

#include <stdio.h>
#include <string.h>

#ifdef __cplusplus
#include <stdexcept>

static bool thrown_and_caught()
{
    try {
        throw std::runtime_error("static");
    } catch (const std::runtime_error &) {
        return true;
    }
    return false;
}
#endif

int main(void)
{
    uint32_t version = lowlat_abi_version();
    if ((version >> 16) != LOWLAT_ABI_MAJOR || (version & 0xffff) != LOWLAT_ABI_MINOR) {
        fprintf(stderr, "static: the library reports version %u.%u, the header says %u.%u\n",
                version >> 16, version & 0xffff,
                (unsigned) LOWLAT_ABI_MAJOR, (unsigned) LOWLAT_ABI_MINOR);
        return 1;
    }
    if ((lowlat_features() & LOWLAT_FEATURE_CLIENT) == 0) {
        fprintf(stderr, "static: the library carries no client half (features 0x%x)\n",
                (unsigned) lowlat_features());
        return 1;
    }

    lowlat_client_create_info info;
    memset(&info, 0, sizeof info);
    info.size = (uint32_t) sizeof info;
    info.decoder = LOWLAT_DECODER_NONE;
    lowlat_client *cl = NULL;
    if (lowlat_client_create(&info, &cl) != LOWLAT_OK || cl == NULL) {
        fprintf(stderr, "static: a client handle could not be created\n");
        return 1;
    }
    lowlat_credentials ours;
    memset(&ours, 0, sizeof ours);
    ours.size = (uint32_t) sizeof ours;
    if (lowlat_client_new_attempt(cl, NULL, "attempt", LOWLAT_TRANSPORT_BUD, &ours) != LOWLAT_OK
        || strlen(ours.ufrag) != 8 || strlen(ours.aes256) != 254) {
        fprintf(stderr, "static: a client attempt produced no credentials\n");
        return 1;
    }
    lowlat_client_end_connection(cl);

#ifdef __cplusplus
    if (!thrown_and_caught()) {
        fprintf(stderr, "static: an exception before the panic was not caught\n");
        return 1;
    }
#endif
    lowlat_status contained = lowlat_client_debug_panic(cl);
#ifdef __cplusplus
    if (!thrown_and_caught()) {
        fprintf(stderr, "static: an exception after the panic was not caught\n");
        return 1;
    }
#endif
    if (contained != LOWLAT_ERR_INTERNAL) {
        fprintf(stderr, "static: a deliberate client panic returned %d, not %d\n",
                (int) contained, (int) LOWLAT_ERR_INTERNAL);
        return 1;
    }
    lowlat_client_status standing;
    memset(&standing, 0, sizeof standing);
    standing.size = (uint32_t) sizeof standing;
    if (lowlat_client_get_status(cl, &standing) != LOWLAT_ERR_POISONED) {
        fprintf(stderr, "static: a client call after a contained panic was not refused\n");
        return 1;
    }
    lowlat_client_destroy(cl);

    printf("static: version %u.%u features 0x%x, a panic returned %d (%s)\n", version >> 16,
           version & 0xffff, (unsigned) lowlat_features(), (int) contained,
           lowlat_status_string(contained));
    return 0;
}
