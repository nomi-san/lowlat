#!/usr/bin/env bash
# Regenerate the system video decoding bindings from the Windows SDK headers.
#
# Run by hand on Windows, never from a build script: the generated files are
# committed so that no build machine needs a C toolchain or the SDK. Run from
# a shell that sees LLVM's clang and the SDK (a Visual Studio x64 developer
# environment does):
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-d3d11-bindings.sh
#
# **The SDK's headers are not vendored**, so the SDK installed on the machine
# is the source; its version is written into the generated file's first
# lines, and a regeneration against another one shows in the diff. Every
# struct carries a compile-time layout assertion derived from the header, so
# a transcription error is a build failure.
#
# The interfaces are declared in their C form: a structure holding a pointer
# to a table of function pointers, called through that table. Nothing is
# linked; the libraries are opened at run time.

set -euo pipefail

# Git Bash rewrites arguments that look like paths, a leading "//" included,
# which would mangle the generated file's comment line.
export MSYS2_ARG_CONV_EXCL='//'
# The first interpreter that runs: on Windows `python3` may be a store stub.
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/crates/drivers/src/ffi/windows"
kits="${WindowsSdkDir:-C:/Program Files (x86)/Windows Kits/10/}"
version="${WindowsSDKVersion:-10.0.26100.0}"
version="${version%\\}"
version="${version%/}"
inc="$kits/Include/$version"
[ -f "$inc/um/d3d11.h" ] || { echo "no SDK headers at $inc" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The kernel's adapter header uses the status type without declaring it; the
# system's own declaration is one line and the rest of its header would drag
# in the kernel's world.
cat > "$work/d3d11.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <d3d11_4.h>
#include <dxgi1_6.h>
#include <dxva.h>
typedef LONG NTSTATUS;
#include <d3dkmthk.h>
EOF

# Interfaces reached only by pointer from a table the backend calls, and never
# called themselves: opaque, so their own tables are not generated, and
# allowlisted as well, since an opaque type nothing allowlists is not emitted.
opaque=(
    'IDXGI(Surface|Output|SwapChain|SwapChain1|Adapter)'
    'ID3D11(DepthStencilState|BlendState|RasterizerState|Buffer|Texture1D|Texture3D)'
    'ID3D11(ShaderResourceView|RenderTargetView|DepthStencilView|UnorderedAccessView)'
    'ID3D11(Vertex|Hull|Domain|Geometry|Pixel|Compute)Shader'
    'ID3D11(InputLayout|SamplerState|Asynchronous|Query|Predicate|Counter)'
    'ID3D11(ClassInstance|ClassLinkage|CommandList)'
    'ID3D11(VideoProcessor|VideoProcessorEnumerator|VideoProcessorInputView|VideoProcessorOutputView)'
    'ID3D11(AuthenticatedChannel|CryptoSession)'
)
opaque_args=()
for pattern in "${opaque[@]}"; do
    opaque_args+=(--opaque-type "$pattern" --allowlist-type "$pattern")
done

echo "generating d3d11 bindings against SDK $version"
bindgen "$work/d3d11.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    --merge-extern-blocks \
    --wrap-unsafe-ops \
    --raw-line "// Windows SDK $version." \
    `# The interfaces called: the device and its immediate context, the video` \
    `# device and context, the decoder and its output view, the textures, and` \
    `# the adapter walk.` \
    --allowlist-type 'ID3D11(Device|DeviceContext|Texture2D|VideoDevice|VideoContext|VideoDecoder|VideoDecoderOutputView)' \
    --allowlist-type 'IDXGI(Factory1|Factory6|Adapter1)'     --allowlist-type 'DXGI_GPU_PREFERENCE' \
    `# The fence a picture's device work is known finished by, the device and` \
    `# context that make and signal it, and a shared texture's handle.` \
    --allowlist-type 'ID3D11(Device5|DeviceContext4|Fence)' \
    --allowlist-type 'IDXGIResource' \
    `# The two entry points resolved at run time, and the kernel's adapter type.` \
    --allowlist-type 'PFN_D3D11_CREATE_DEVICE' \
    --allowlist-type 'PFND3DKMT_(OPENADAPTERFROMLUID|QUERYADAPTERINFO|CLOSEADAPTER)' \
    --allowlist-type 'D3DKMT_ADAPTERTYPE|KMTQUERYADAPTERINFOTYPE' \
    `# The picture, matrix and slice structures of both codecs, the range` \
    `# extension's included.` \
    --allowlist-type 'DXVA_(PicParams_H264|Qmatrix_H264|Slice_H264_Short)' \
    --allowlist-type 'DXVA_(PicParams_HEVC_RangeExt|Qmatrix_HEVC|Slice_HEVC_Short)' \
    `# Flags passed as plain integers, which nothing above names as a type.` \
    --allowlist-type 'D3D11_(CREATE_DEVICE_FLAG|BIND_FLAG|CPU_ACCESS_FLAG|MAP|USAGE)' \
    --allowlist-type 'D3D11_(RESOURCE_MISC_FLAG|FENCE_FLAG|MAP_FLAG|SRV_DIMENSION|UAV_DIMENSION)' \
    --allowlist-type 'D3D11_(VIDEO_DECODER_BUFFER_TYPE|VDOV_DIMENSION)' \
    --allowlist-var 'D3D11_SDK_VERSION' \
    "${opaque_args[@]}" \
    -o "$out/d3d11.rs" \
    -- --target=x86_64-pc-windows-msvc

# Interface identifiers and decoder profiles, transcribed mechanically from
# the header text so that no human copies sixteen bytes of hex. The headers
# declare both as external objects that only an import library defines,
# which nothing here links.
echo "generating d3d11 identifiers"
"$py" - "$inc" "$out/d3d11_guids.rs" "$version" <<'PY'
import re
import sys

inc, dst, version = sys.argv[1], sys.argv[2], sys.argv[3]

def read(rel):
    return open(inc + "/" + rel, encoding="utf-8", errors="replace").read()

# The interfaces whose identifiers are asked for by name.
interfaces = {
    "um/d3d11.h": ["ID3D11Device", "ID3D11VideoDevice", "ID3D11VideoContext",
                   "ID3D11Texture2D"],
    "um/d3d11_3.h": ["ID3D11Fence", "ID3D11DeviceContext4"],
    "um/d3d11_4.h": ["ID3D11Device5"],
    "shared/dxgi.h": ["IDXGIFactory1", "IDXGIAdapter1", "IDXGIDevice", "IDXGIResource"],
    "shared/dxgi1_6.h": ["IDXGIFactory6"],
}
# The decoder profiles every codec here is opened with.
profiles = re.compile(r"D3D11_DECODER_PROFILE_(H264_VLD_NOFGT|HEVC_VLD_\w+)$")

lines = [
    "// Generated by scripts/gen-d3d11-bindings.sh. Do not edit.",
    "// Windows SDK %s." % version,
    "",
    "use super::d3d11::GUID;",
    "",
]

def emit(name, d1, d2, d3, octets):
    if len(octets) != 8:
        raise SystemExit("guid %s has %d octets, expected 8" % (name, len(octets)))
    lines.append("pub const %s: GUID = GUID {" % name)
    lines.append("    Data1: 0x%08x," % d1)
    lines.append("    Data2: 0x%04x," % d2)
    lines.append("    Data3: 0x%04x," % d3)
    lines.append("    Data4: [%s]," % ", ".join("0x%02x" % o for o in octets))
    lines.append("};")

count = 0
for header, names in interfaces.items():
    text = read(header)
    for name in names:
        m = re.search(r'MIDL_INTERFACE\("([0-9A-Fa-f-]{36})"\)\s*\n\s*%s\s*:' % name, text)
        if not m:
            raise SystemExit("no identifier for %s in %s" % (name, header))
        h = m.group(1).replace("-", "")
        emit("IID_" + name, int(h[0:8], 16), int(h[8:12], 16), int(h[12:16], 16),
             [int(h[i:i + 2], 16) for i in range(16, 32, 2)])
        count += 1

text = read("um/d3d11.h")
for m in re.finditer(r"DEFINE_GUID\((\w+),\s*([^)]*)\)", text):
    name = m.group(1)
    if not profiles.search(name):
        continue
    parts = [int(p.strip(), 16) for p in m.group(2).split(",")]
    emit(name, parts[0], parts[1], parts[2], parts[3:])
    count += 1

if count < 8:
    raise SystemExit("only %d identifiers matched; the header layout changed" % count)

open(dst, "w", encoding="ascii", newline="\n").write("\n".join(lines) + "\n")
print("  %d identifiers" % count)
PY

# The workspace's formatting, which `cargo fmt --check` holds the committed
# files to; the generator's own pass formats one construct differently.
rustfmt --edition 2024 "$out/d3d11.rs" "$out/d3d11_guids.rs"
