#!/usr/bin/env bash
# Regenerate Intel's video runtime bindings from the vendored headers.
#
# Run by hand on Windows, never from a build script: the generated file is
# committed so that no build machine needs a C toolchain. Run from a shell that
# sees LLVM's clang (a Visual Studio x64 developer environment does):
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-vpl-bindings.sh
#
# The headers are vendored (third_party/vpl, see its PROVENANCE.md), so a
# regeneration against the same pin reproduces the file. Types and constants
# only: the runtime is opened at run time and its entry points resolved by
# name, so no function is declared here. The structures are packed by the
# header's own rules, four bytes and eight for those holding a pointer on
# 64-bit Windows, which the target below makes clang apply; every struct carries
# a compile-time layout assertion derived from the header.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
inc="$root/third_party/vpl/include"
out="$root/crates/drivers/src/ffi/windows"
[ -f "$inc/vpl/mfxvideo.h" ] || { echo "no VPL headers at $inc" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
cat > "$work/vpl.h" <<'EOF'
#include <vpl/mfxvideo.h>
EOF

echo "generating vpl bindings"
bindgen "$work/vpl.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    --merge-extern-blocks \
    --wrap-unsafe-ops \
    --raw-line "// libvpl headers, third_party/vpl (see its PROVENANCE.md)." \
    `# A session, its initialisation both ways, the threads it starts, and the` \
    `# device handed to it.` \
    --allowlist-type 'mfx(Session|Status|IMPL|Version|InitParam|InitializationParam)' \
    --allowlist-type 'mfx(ExtBuffer|ExtThreadsParam|HDL|HandleType|ResourceType|SyncPoint)' \
    `# A decoder's parameters, its units in and its pictures out.` \
    --allowlist-type 'mfx(VideoParam|InfoMFX|FrameInfo|FrameId|FrameAllocRequest|Bitstream)' \
    --allowlist-type 'mfx(FrameSurface1|FrameSurfaceInterface|FrameData)' \
    `# The constants those are filled and answered with.` \
    --allowlist-item 'MFX_(ERR|WRN)_.*' \
    --allowlist-item 'MFX_(CODEC|FOURCC|CHROMAFORMAT|PICSTRUCT|IOPATTERN|BITSTREAM)_.*' \
    --allowlist-item 'MFX_(IMPL|ACCEL_MODE|HANDLE|RESOURCE)_.*' \
    --allowlist-item 'MFX_(PROFILE_AVC|PROFILE_HEVC|EXTBUFF_THREADS_PARAM).*' \
    -o "$out/vpl.rs" \
    -- --target=x86_64-pc-windows-msvc -I "$inc"

# The workspace's formatting, which `cargo fmt --check` holds the committed
# file to.
rustfmt --edition 2024 "$out/vpl.rs"
