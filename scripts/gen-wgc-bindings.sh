#!/usr/bin/env bash
# Regenerate the bindings of the system's capture of a window from the Windows
# SDK headers: the capture item, its frame pool, its frames and its session,
# the two event handlers this side implements, the interface the system hands
# a window to, and the Direct3D device and surface the frames come in.
#
# Run by hand on Windows, never from a build script: the generated files are
# committed so that no build machine needs a C toolchain or the SDK. Run from
# a shell that sees LLVM's clang and the SDK:
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-wgc-bindings.sh
#
# As scripts/gen-d3d11-bindings.sh: the installed SDK is the source, its
# version written into the generated files; every struct carries a layout
# assertion; the interfaces are declared in their C form; nothing is linked.
#
# **Two interfaces are declared for C++ alone**, so their tables are written by
# hand beside these (crates/drivers/src/wgc.rs) and only their identifiers are
# transcribed here: the one a frame's surface hands its texture over by, which
# has no C form at all, and the one a window is handed to, whose methods are
# declared without the object argument -- a C table read from it would pass the
# window where the object belongs.

set -euo pipefail

export MSYS2_ARG_CONV_EXCL='//'
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
[ -f "$inc/winrt/windows.graphics.capture.h" ] || { echo "no SDK headers at $inc" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/wgc.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <windows.graphics.capture.h>
#include <Windows.Graphics.Capture.Interop.h>
EOF

# The one interface reached only by pointer from a table this side calls, and
# never called: opaque, so the interfaces its own table names are not
# generated, and allowlisted, since an opaque type nothing allowlists is not
# emitted. By its exact name: a pattern is matched against whole names and
# allowlists every one it matches, and the system's namespace has hundreds.
opaque_args=(
    --opaque-type '__x_ABI_CWindows_CSystem_CIDispatcherQueue'
    --allowlist-type '__x_ABI_CWindows_CSystem_CIDispatcherQueue'
)

echo "generating capture bindings against SDK $version"
bindgen "$work/wgc.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    --merge-extern-blocks \
    --wrap-unsafe-ops \
    --raw-line "// Windows SDK $version." \
    `# The item a window becomes, the frame pool and its frames, the session` \
    `# and the four of its later interfaces whose properties are set: the` \
    `# cursor, the border, the interval, the secondary windows.` \
    --allowlist-type '__x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureItem' \
    --allowlist-type '__x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSession[2356]?' \
    --allowlist-type '__x_ABI_CWindows_CGraphics_CCapture_CIGraphicsCaptureSessionStatics' \
    --allowlist-type '__x_ABI_CWindows_CGraphics_CCapture_CIDirect3D11CaptureFrame(Pool|PoolStatics2)?' \
    `# The two handlers this side implements: a frame arrived, the item closed.` \
    --allowlist-type '__FITypedEventHandler_2_Windows__CGraphics__CCapture__C(Direct3D11CaptureFramePool|GraphicsCaptureItem)_IInspectable' \
    `# What closes each object, and the device and surface the frames are in.` \
    --allowlist-type '__x_ABI_CWindows_CFoundation_CIClosable' \
    --allowlist-type '__x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_CIDirect3D(Device|Surface)' \
    --allowlist-type '__x_ABI_CWindows_CGraphics_CDirectX_CDirectXPixelFormat' \
    "${opaque_args[@]}" \
    -o "$out/wgc.rs" \
    -- --target=x86_64-pc-windows-msvc

# Identifiers and runtime class names, transcribed mechanically from the header
# text: the headers declare the identifiers as external objects that only an
# import library defines, which nothing here links.
echo "generating capture identifiers"
"$py" - "$inc" "$out/wgc_guids.rs" "$version" <<'PY'
import re
import sys

inc, dst, version = sys.argv[1], sys.argv[2], sys.argv[3]

def read(rel):
    return open(inc + "/" + rel, encoding="utf-8", errors="replace").read()

CAPTURE = "__x_ABI_CWindows_CGraphics_CCapture_C"
DIRECT3D = "__x_ABI_CWindows_CGraphics_CDirectX_CDirect3D11_C"
# header -> (the C++ name the identifier precedes, the name it is emitted by)
named = {
    "winrt/windows.graphics.capture.h": [
        (n, CAPTURE + n) for n in [
            "IGraphicsCaptureItem", "IGraphicsCaptureSession", "IGraphicsCaptureSession2",
            "IGraphicsCaptureSession3", "IGraphicsCaptureSession5", "IGraphicsCaptureSession6",
            "IGraphicsCaptureSessionStatics", "IDirect3D11CaptureFramePool",
            "IDirect3D11CaptureFramePoolStatics2", "IDirect3D11CaptureFrame",
        ]
    ],
    "winrt/windows.foundation.h": [("IClosable", "__x_ABI_CWindows_CFoundation_CIClosable")],
    "winrt/windows.graphics.directx.direct3d11.h": [
        ("IDirect3DDevice", DIRECT3D + "IDirect3DDevice"),
        ("IDirect3DSurface", DIRECT3D + "IDirect3DSurface"),
    ],
    "um/windows.graphics.directx.direct3d11.interop.h": [
        ("IDirect3DDxgiInterfaceAccess", "IDirect3DDxgiInterfaceAccess"),
    ],
    "um/objidl.h": [("IAgileObject", "IAgileObject")],
    "um/unknwnbase.h": [("IUnknown", "IUnknown")],
}
# The two handlers, parameterised: identified by their arguments.
handlers = [
    ("Direct3D11CaptureFramePool",
     "__FITypedEventHandler_2_Windows__CGraphics__CCapture__CDirect3D11CaptureFramePool_IInspectable"),
    ("GraphicsCaptureItem",
     "__FITypedEventHandler_2_Windows__CGraphics__CCapture__CGraphicsCaptureItem_IInspectable"),
]

lines = [
    "// Generated by scripts/gen-wgc-bindings.sh. Do not edit.",
    "// Windows SDK %s." % version,
    "",
    "use super::wgc::GUID;",
    "",
]

def emit(name, text):
    h = text.replace("-", "")
    d1, d2, d3 = int(h[0:8], 16), int(h[8:12], 16), int(h[12:16], 16)
    octets = [int(h[i:i + 2], 16) for i in range(16, 32, 2)]
    lines.append("pub const IID_%s: GUID = GUID {" % name)
    lines.append("    Data1: 0x%08x," % d1)
    lines.append("    Data2: 0x%04x," % d2)
    lines.append("    Data3: 0x%04x," % d3)
    lines.append("    Data4: [%s]," % ", ".join("0x%02x" % o for o in octets))
    lines.append("};")

count = 0
for header, names in named.items():
    text = read(header)
    for cxx, name in names:
        # The name ends at a word's end: IUnknown has no base to follow it.
        m = re.search(r'(?:MIDL_INTERFACE|struct __declspec\(uuid)\("([0-9A-Fa-f-]{36})"\)\)?\s*\n\s*%s\b'
                      % re.escape(cxx), text)
        if not m:
            raise SystemExit("no identifier for %s in %s" % (cxx, header))
        emit(name, m.group(1))
        count += 1

text = read("um/Windows.Graphics.Capture.Interop.h")
m = re.search(r'DECLARE_INTERFACE_IID_\(IGraphicsCaptureItemInterop,\s*\w+,\s*"([0-9A-Fa-f-]{36})"\)', text)
if not m:
    raise SystemExit("no identifier for IGraphicsCaptureItemInterop")
emit("IGraphicsCaptureItemInterop", m.group(1))
count += 1

text = read("winrt/windows.graphics.capture.h")
for argument, name in handlers:
    m = re.search(r'struct __declspec\(uuid\("([0-9A-Fa-f-]{36})"\)\)\s*\n'
                  r'ITypedEventHandler<ABI::Windows::Graphics::Capture::%s\*, IInspectable\*>'
                  % argument, text)
    if not m:
        raise SystemExit("no identifier for the handler of %s" % argument)
    emit(name, m.group(1))
    count += 1

lines.append("")
classes = sorted(set(re.findall(
    r'RuntimeClass_(Windows_Graphics_Capture_(?:GraphicsCaptureItem|GraphicsCaptureSession|'
    r'Direct3D11CaptureFramePool))\[\] = L"([^"]+)"', text)))
if len(classes) != 3:
    raise SystemExit("found %d runtime class names, expected 3" % len(classes))
for name, value in classes:
    lines.append("pub const RuntimeClass_%s: &str = \"%s\";" % (name, value))

open(dst, "w", encoding="ascii", newline="\n").write("\n".join(lines) + "\n")
print("  %d identifiers, %d class names" % (count, len(classes)))
PY

rustfmt --edition 2024 "$out/wgc.rs" "$out/wgc_guids.rs"
