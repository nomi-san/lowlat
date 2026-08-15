#!/usr/bin/env bash
# Capture backend probe. Resolves docs/07-platforms.md 3.
#
# Run this on the target Linux machine, on bare metal, with a display attached
# and a session running. It needs no lowlat code and answers six open items
# across four documents.
#
#   ./scripts/probe-capture.sh            # read-only stages plus encode test
#   sudo ./scripts/probe-capture.sh       # all stages, including scanout
#
# Stages 4 and later need CAP_SYS_ADMIN to read another process's framebuffer.
# That privilege requirement is itself one of the findings.

set -u

# A non-login shell omits sbin, where modprobe lives.
PATH="$PATH:/usr/sbin:/sbin"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0
SKIP=0

# Findings consumed by the verdict.
F_ENCODE=skip
ENCODER=""
HWDEV=""
F_SCANOUT_EXPORT=skip
F_SCANOUT_MAP=skip
F_SCANOUT_CUDA=skip
F_VKMS_EXPORT=skip

say()  { printf '%s\n' "$*"; }
head2() { printf '\n== %s ==\n' "$*"; }

result() {
    # result <PASS|FAIL|SKIP> <label> [detail]
    case "$1" in
        PASS) PASS=$((PASS + 1)) ;;
        FAIL) FAIL=$((FAIL + 1)) ;;
        SKIP) SKIP=$((SKIP + 1)) ;;
    esac
    printf '  [%-4s] %s\n' "$1" "$2"
    if [ $# -gt 2 ] && [ -n "$3" ]; then
        printf '         %s\n' "$3"
    fi
}

tail_err() {
    # Last meaningful line of a captured stderr file.
    grep -viE '^\s*$|^ *(built with|configuration:|lib[a-z]+ +[0-9])' "$1" 2>/dev/null \
        | tail -n 2 | tr '\n' ' ' | cut -c1-160
}

have() { command -v "$1" >/dev/null 2>&1; }

is_root() { [ "$(id -u)" -eq 0 ]; }

# ---------------------------------------------------------------- stage 0

head2 "Stage 0: environment"

say "  kernel      $(uname -r)"
say "  distro      $(. /etc/os-release 2>/dev/null && echo "${PRETTY_NAME:-unknown}")"
say "  privileged  $(is_root && echo yes || echo 'no (stages 4+ will skip)')"
say "  session     ${XDG_SESSION_TYPE:-none}${WAYLAND_DISPLAY:+ (wayland)}"

for t in ffmpeg nvidia-smi modprobe; do
    have "$t" && say "  tool $t: $(command -v $t)" || say "  tool $t: MISSING"
done

# ---------------------------------------------------------------- stage 1

head2 "Stage 1: display devices"

if [ -d /sys/class/drm ] && ls /sys/class/drm/card* >/dev/null 2>&1; then
    for c in /sys/class/drm/card[0-9]*; do
        [ -e "$c/device/driver" ] || continue
        drv="$(basename "$(readlink -f "$c/device/driver")")"
        say "  $(basename "$c")  driver=$drv"
    done
    for c in /sys/class/drm/card*-*/status; do
        [ -r "$c" ] || continue
        st="$(cat "$c")"
        [ "$st" = "connected" ] && say "  connector $(basename "$(dirname "$c")")  $st"
    done
    result PASS "display devices present"
else
    result FAIL "no display devices" "/sys/class/drm has no cards; this is not bare metal"
fi

# ---------------------------------------------------------------- stage 2

head2 "Stage 2: modesetting on the proprietary driver"

MODESET=/sys/module/nvidia_drm/parameters/modeset
if [ ! -e "$MODESET" ]; then
    result SKIP "proprietary driver not loaded" "nvidia_drm absent; scanout may still work on the open stack"
elif [ "$(cat "$MODESET" 2>/dev/null)" = "Y" ]; then
    result PASS "nvidia_drm modeset enabled"
else
    result FAIL "nvidia_drm modeset disabled" \
        "scanout cannot work. Set nvidia-drm.modeset=1 and reboot, then rerun."
fi

# ---------------------------------------------------------------- stage 3

head2 "Stage 3: hardware encode (control)"

# Being listed is not being usable: a distribution ffmpeg advertises NVENC
# encoders on a machine with no NVIDIA hardware at all. So each candidate is
# tried and the first that actually produces frames wins.
try_encode() {
    # try_encode <encoder> <hwdev>
    case "$2" in
        vaapi)
            ffmpeg -hide_banner -loglevel error -init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va -f lavfi -i testsrc=size=1920x1080:rate=60 -frames:v 60 -vf "format=nv12,hwupload" -c:v "$1" -f null - >"$WORK/enc.err" 2>&1
            ;;
        *)
            ffmpeg -hide_banner -loglevel error -f lavfi -i testsrc=size=1920x1080:rate=60 -frames:v 60 -c:v "$1" -f null - >"$WORK/enc.err" 2>&1
            ;;
    esac
}

if ! have ffmpeg; then
    result SKIP "ffmpeg missing" "install ffmpeg to run the remaining stages"
else
    ENCODERS=$(ffmpeg -hide_banner -encoders 2>/dev/null || true)
    LAST_ERR=""
    for candidate in "h264_nvenc cuda" "h264_vaapi vaapi"; do
        # shellcheck disable=SC2086
        set -- $candidate
        printf '%s' "$ENCODERS" | grep -q "$1" || continue
        if try_encode "$1" "$2"; then
            ENCODER="$1"; HWDEV="$2"
            break
        fi
        LAST_ERR="$1: $(tail_err "$WORK/enc.err")"
    done

    if [ -n "$ENCODER" ]; then
        result PASS "hardware encode works ($ENCODER)" "synthetic 1080p60, 60 frames"
        F_ENCODE=pass
    else
        result FAIL "no usable hardware encoder" "${LAST_ERR:-none of h264_nvenc, h264_vaapi are built in}"
        F_ENCODE=fail
    fi
fi


# ---------------------------------------------------------------- stage 4

head2 "Stage 4: scanout capture and buffer export"

CARD=""
for c in /dev/dri/card0 /dev/dri/card1 /dev/dri/card2; do
    [ -e "$c" ] && CARD="$c" && break
done

if ! have ffmpeg; then
    result SKIP "ffmpeg missing" ""
elif [ -z "$CARD" ]; then
    result SKIP "no card node" ""
elif ! is_root; then
    result SKIP "needs privilege" "rerun with sudo to test scanout"
else
    # 4a: capture and export. No mapping, no import. Tests the framebuffer
    # fetch and the shareable-handle export only.
    if ffmpeg -hide_banner -loglevel error -f kmsgrab -device "$CARD" \
        -framerate 30 -i - -frames:v 30 -f null - >"$WORK/kms.err" 2>&1; then
        result PASS "framebuffer fetch and export" "30 frames from $CARD"
        F_SCANOUT_EXPORT=pass
    else
        result FAIL "framebuffer fetch or export failed" "$(tail_err "$WORK/kms.err")"
        F_SCANOUT_EXPORT=fail
    fi

    # 4b: can the exported buffer be mapped at all. Tiled or vendor-specific
    # layouts commonly fail here while 4a succeeds.
    if [ "$F_SCANOUT_EXPORT" = pass ]; then
        if ffmpeg -hide_banner -loglevel error -f kmsgrab -device "$CARD" \
            -framerate 30 -i - -vf 'hwdownload,format=bgr0' -frames:v 5 \
            -f null - >"$WORK/kmsmap.err" 2>&1; then
            result PASS "exported buffer is mappable"
            F_SCANOUT_MAP=pass
        else
            result FAIL "exported buffer not mappable" "$(tail_err "$WORK/kmsmap.err")"
            F_SCANOUT_MAP=fail
        fi
    fi
fi

# ---------------------------------------------------------------- stage 5

head2 "Stage 5: zero-copy import into the encoder's context"

if [ "$F_SCANOUT_EXPORT" != pass ]; then
    result SKIP "scanout export did not succeed" ""
elif [ "$F_ENCODE" != pass ]; then
    result SKIP "no working hardware encoder" ""
else
    # THE decisive test: capture -> shareable handle -> the encoder's context ->
    # encode, with no download to system memory anywhere in the chain.
    if [ "$HWDEV" = vaapi ]; then
        ZC_ARGS="-init_hw_device vaapi=va:/dev/dri/renderD128 -filter_hw_device va"
        ZC_FILTER="hwmap=derive_device=vaapi,scale_vaapi=format=nv12"
    else
        ZC_ARGS="-init_hw_device cuda=cu -filter_hw_device cu"
        ZC_FILTER="hwmap=derive_device=cuda,scale_cuda=format=nv12"
    fi
    # shellcheck disable=SC2086
    if ffmpeg -hide_banner -loglevel error $ZC_ARGS         -f kmsgrab -device "$CARD" -framerate 60 -i -         -vf "$ZC_FILTER"         -frames:v 60 -c:v "$ENCODER" -f null - >"$WORK/zerocopy.err" 2>&1; then
        result PASS "zero-copy capture to encode ($HWDEV)" "this is the v1 scanout path"
        F_SCANOUT_CUDA=pass
    else
        result FAIL "zero-copy import failed ($HWDEV)" "$(tail_err "$WORK/zerocopy.err")"
        F_SCANOUT_CUDA=fail
    fi
fi


# ---------------------------------------------------------------- stage 6

head2 "Stage 6: virtual display (control, and a candidate backend)"

if ! is_root; then
    result SKIP "needs privilege" ""
elif ! have modprobe; then
    result SKIP "modprobe missing" ""
else
    VKMS_WAS_LOADED=no
    lsmod 2>/dev/null | grep -q '^vkms' && VKMS_WAS_LOADED=yes

    if [ "$VKMS_WAS_LOADED" = no ] && ! modprobe vkms 2>"$WORK/vkms.err"; then
        result SKIP "vkms unavailable" "$(tail_err "$WORK/vkms.err")"
    else
        VCARD=""
        for c in /sys/class/drm/card[0-9]*; do
            [ -e "$c/device/driver" ] || continue
            if [ "$(basename "$(readlink -f "$c/device/driver")")" = "vkms" ]; then
                VCARD="/dev/dri/$(basename "$c")"
                break
            fi
        done

        if [ -z "$VCARD" ]; then
            result SKIP "vkms loaded but no card node appeared" ""
        elif ! have ffmpeg; then
            result SKIP "ffmpeg missing" ""
        elif ffmpeg -hide_banner -loglevel error -f kmsgrab -device "$VCARD" \
            -framerate 30 -i - -frames:v 10 -f null - >"$WORK/vkmsgrab.err" 2>&1; then
            result PASS "virtual display capture works" "$VCARD"
            F_VKMS_EXPORT=pass
        else
            result FAIL "virtual display capture failed" "$(tail_err "$WORK/vkmsgrab.err")"
            F_VKMS_EXPORT=fail
        fi

        [ "$VKMS_WAS_LOADED" = no ] && modprobe -r vkms 2>/dev/null
    fi
fi

# ---------------------------------------------------------------- verdict

head2 "Verdict"

printf '  %-34s %s\n' "hardware encode"            "$F_ENCODE ${ENCODER:+($ENCODER)}"
printf '  %-34s %s\n' "scanout export"             "$F_SCANOUT_EXPORT"
printf '  %-34s %s\n' "scanout mappable"           "$F_SCANOUT_MAP"
printf '  %-34s %s\n' "scanout zero-copy to encode" "$F_SCANOUT_CUDA"
printf '  %-34s %s\n' "virtual display export"     "$F_VKMS_EXPORT"
printf '\n  %d passed, %d failed, %d skipped\n\n' "$PASS" "$FAIL" "$SKIP"

if [ "$F_SCANOUT_EXPORT" = pass ] && [ "$F_SCANOUT_CUDA" = pass ]; then
    say "  OUTCOME 1: scanout is the v1 capture backend."
    say "  Physical-screen capture, unattended, greeter-capable, zero-copy."
    say "  Add the session-side pointer probe (07-platforms 2.1)."
    say "  Compositor-mediated becomes the v1.x path for locked-down desktops."
elif [ "$F_SCANOUT_EXPORT" = pass ] && [ "$F_SCANOUT_CUDA" = fail ]; then
    say "  OUTCOME 1b: scanout captures, but not zero-copy into the encoder."
    say "  Do NOT accept a system-memory download as the shipping path."
    say "  Investigate an intermediate graphics-API import before choosing;"
    say "  if none works, treat this as OUTCOME 3."
elif [ "$F_SCANOUT_EXPORT" = fail ] && [ "$F_VKMS_EXPORT" = pass ]; then
    say "  OUTCOME 2: virtual display is the v1 backend."
    say "  The product is a headless host rather than a screen mirror."
    say "  Pointer state becomes exact rather than inferred."
    say "  Physical-screen capture waits for compositor-mediated."
elif [ "$F_SCANOUT_EXPORT" = fail ] && [ "$F_VKMS_EXPORT" = fail ]; then
    say "  OUTCOME 3: compositor-mediated becomes v1."
    say "  The daemon gains a MANDATORY session helper (07-platforms 5),"
    say "  capture lives in the session, and the stream does NOT survive logout."
    say "  Record the reason and defer unattended operation."
else
    say "  INCONCLUSIVE. Rerun with sudo on bare metal with a display attached."
    say "  Stages 4 and later are the ones that decide this."
fi

say ""
say "  Record the result in local/re-delta.md and update 07-platforms 11."

[ "$FAIL" -eq 0 ]
