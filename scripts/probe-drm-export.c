/* Direct scanout export probe. Answers docs/07-platforms.md 3 steps 2 and 3
 * without ffmpeg and without any development package.
 *
 * ffmpeg's kmsgrab refuses a framebuffer whose pixel format it lacks a mapping
 * for, and it does so before attempting the export, so a failure there says
 * nothing about whether the export works. This asks the kernel directly.
 *
 *   gcc -O2 -o probe-drm-export probe-drm-export.c
 *   sudo ./probe-drm-export [/dev/dri/card0]
 *
 * The DRM ioctl numbers and structures are stable kernel ABI, so they are
 * declared inline rather than pulled from headers that may not be installed.
 */

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

struct dr_set_client_cap {
    uint64_t capability;
    uint64_t value;
};

struct dr_plane_res {
    uint64_t plane_id_ptr;
    uint32_t count_planes;
};

struct dr_get_plane {
    uint32_t plane_id;
    uint32_t crtc_id;
    uint32_t fb_id;
    uint32_t possible_crtcs;
    uint32_t gamma_size;
    uint32_t count_format_types;
    uint64_t format_type_ptr;
};

struct dr_fb_cmd2 {
    uint32_t fb_id;
    uint32_t width;
    uint32_t height;
    uint32_t pixel_format;
    uint32_t flags;
    uint32_t handles[4];
    uint32_t pitches[4];
    uint32_t offsets[4];
    uint64_t modifier[4];
};

struct dr_prime_handle {
    uint32_t handle;
    uint32_t flags;
    int32_t fd;
};

#define DRM_IOCTL_SET_CLIENT_CAP _IOW('d', 0x0d, struct dr_set_client_cap)
#define DRM_IOCTL_PRIME_HANDLE_TO_FD _IOWR('d', 0x2d, struct dr_prime_handle)
#define DRM_IOCTL_MODE_GETPLANERESOURCES _IOWR('d', 0xb5, struct dr_plane_res)
#define DRM_IOCTL_MODE_GETPLANE _IOWR('d', 0xb6, struct dr_get_plane)
#define DRM_IOCTL_MODE_GETFB2 _IOWR('d', 0xce, struct dr_fb_cmd2)

#define DRM_CLIENT_CAP_UNIVERSAL_PLANES 2
#define DRM_CLOEXEC 0x80000
#define DRM_RDWR 0x8000

static void fourcc(uint32_t v, char out[5])
{
    out[0] = (char)(v & 0xff);
    out[1] = (char)((v >> 8) & 0xff);
    out[2] = (char)((v >> 16) & 0xff);
    out[3] = (char)((v >> 24) & 0xff);
    out[4] = 0;
}

int main(int argc, char **argv)
{
    const char *path = argc > 1 ? argv[1] : "/dev/dri/card0";
    int exported = 0;
    int scanned = 0;

    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd < 0) {
        printf("open %s: %s\n", path, strerror(errno));
        return 2;
    }
    printf("device %s\n", path);

    struct dr_set_client_cap cap = { DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1 };
    if (ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &cap) != 0)
        printf("  note: universal planes unavailable (%s)\n", strerror(errno));

    struct dr_plane_res res;
    memset(&res, 0, sizeof res);
    if (ioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &res) != 0) {
        printf("  GETPLANERESOURCES: %s\n", strerror(errno));
        close(fd);
        return 2;
    }
    if (res.count_planes == 0 || res.count_planes > 64) {
        printf("  implausible plane count %u\n", res.count_planes);
        close(fd);
        return 2;
    }

    uint32_t ids[64];
    memset(ids, 0, sizeof ids);
    res.plane_id_ptr = (uint64_t)(uintptr_t)ids;
    if (ioctl(fd, DRM_IOCTL_MODE_GETPLANERESOURCES, &res) != 0) {
        printf("  GETPLANERESOURCES (fetch): %s\n", strerror(errno));
        close(fd);
        return 2;
    }
    printf("  %u planes\n", res.count_planes);

    for (uint32_t i = 0; i < res.count_planes; i++) {
        struct dr_get_plane plane;
        memset(&plane, 0, sizeof plane);
        plane.plane_id = ids[i];
        if (ioctl(fd, DRM_IOCTL_MODE_GETPLANE, &plane) != 0)
            continue;
        if (plane.fb_id == 0 || plane.crtc_id == 0)
            continue;

        scanned++;
        struct dr_fb_cmd2 fb;
        memset(&fb, 0, sizeof fb);
        fb.fb_id = plane.fb_id;
        if (ioctl(fd, DRM_IOCTL_MODE_GETFB2, &fb) != 0) {
            printf("  plane %u fb %u: GETFB2 failed: %s\n", plane.plane_id,
                   plane.fb_id, strerror(errno));
            continue;
        }

        char cc[5];
        fourcc(fb.pixel_format, cc);
        printf("  plane %u crtc %u fb %u: %ux%u format %s modifier 0x%016llx\n",
               plane.plane_id, plane.crtc_id, fb.fb_id, fb.width, fb.height, cc,
               (unsigned long long)fb.modifier[0]);

        for (int p = 0; p < 4; p++) {
            if (fb.handles[p] == 0)
                continue;
            struct dr_prime_handle prime;
            memset(&prime, 0, sizeof prime);
            prime.handle = fb.handles[p];
            /* Read-only, deliberately. Asking for DRM_RDWR makes amdgpu refuse
             * a scanout buffer outright with EINVAL, and a capture path has no
             * business writing to the framebuffer anyway. This is why ffmpeg's
             * kmsgrab fails here: it requests write access it does not need. */
            prime.flags = DRM_CLOEXEC;
            prime.fd = -1;
            if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &prime) != 0) {
                printf("    plane-buffer %d: EXPORT FAILED: %s\n", p,
                       strerror(errno));
                continue;
            }
            off_t size = lseek(prime.fd, 0, SEEK_END);
            printf("    plane-buffer %d: exported fd, %lld bytes, pitch %u\n", p,
                   (long long)size, fb.pitches[p]);
            close(prime.fd);
            exported++;
        }
    }

    close(fd);
    printf("\nresult: %d framebuffer(s) scanned, %d buffer(s) exported\n", scanned,
           exported);
    if (exported > 0) {
        printf("VERDICT: scanout capture and buffer export both work here.\n");
        return 0;
    }
    if (scanned == 0)
        printf("VERDICT: nothing is being scanned out. Is a session running?\n");
    else
        printf("VERDICT: framebuffers found but none could be exported.\n");
    return 1;
}
