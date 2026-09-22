/* Says what a built library is, from the object at the given path.
 *
 * One line, the ABI version and the feature bits, read from the object that
 * will ship rather than from whatever a build left in the target directory:
 * the two builds of the library land on the same file, and the tarball's own
 * copy is the only thing worth asking. The version is checked against the
 * header this was compiled with, which is the header that ships beside it.
 *
 *   describe lib/liblowlat.so       prints "abi 0.13 features 0x3"
 *   describe lib/liblowlat.so 0x2   the same, and fails if the bits differ
 *
 * The gate's harness is not this program: it names an object without the
 * host half and stops, by design.
 */

#include "lowlat.h"

#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* dlsym returns an object pointer and a function pointer is needed; copying
 * the bytes is the one spelling that is not a constraint violation. */
#define RESOLVE(fn, lib, name)                                                 \
    do {                                                                       \
        void *raw = dlsym((lib), (name));                                      \
        if (raw == NULL) {                                                     \
            fprintf(stderr, "describe: %s is not exported\n", (name));         \
            return 2;                                                          \
        }                                                                      \
        memcpy(&(fn), &raw, sizeof raw);                                       \
    } while (0)

int main(int argc, char **argv)
{
    if (argc < 2 || argc > 3) {
        fprintf(stderr, "usage: describe <path to shared object> [expected features]\n");
        return 2;
    }

    void *lib = dlopen(argv[1], RTLD_NOW);
    if (lib == NULL) {
        fprintf(stderr, "describe: dlopen failed: %s\n", dlerror());
        return 2;
    }

    uint32_t (*abi_version)(void);
    uint32_t (*features)(void);
    RESOLVE(abi_version, lib, "lowlat_abi_version");
    RESOLVE(features, lib, "lowlat_features");

    uint32_t version = abi_version();
    uint32_t bits = features();
    printf("abi %u.%u features 0x%x\n", version >> 16, version & 0xffff, (unsigned) bits);

    if ((version >> 16) != LOWLAT_ABI_MAJOR || (version & 0xffff) != LOWLAT_ABI_MINOR) {
        fprintf(stderr, "describe: the object reports version %u.%u, the header says %u.%u\n",
                version >> 16, version & 0xffff,
                (unsigned) LOWLAT_ABI_MAJOR, (unsigned) LOWLAT_ABI_MINOR);
        return 1;
    }
    if (argc == 3 && bits != (uint32_t) strtoul(argv[2], NULL, 0)) {
        fprintf(stderr, "describe: the object reports features 0x%x, expected %s\n",
                (unsigned) bits, argv[2]);
        return 1;
    }
    return 0;
}
