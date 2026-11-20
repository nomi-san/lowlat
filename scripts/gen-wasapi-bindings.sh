#!/usr/bin/env bash
# Regenerate the system sound interface's declarations from the Windows SDK
# headers: the device enumeration, the audio client and its capture and render
# halves, the endpoint's volume and its property store; the sessions each
# output holds, and the capture of one process's sound.
#
# Run by hand on Windows, never from a build script: the generated files are
# committed so that no build machine needs a C toolchain or the SDK. Run from
# a shell that sees LLVM's clang and the SDK (a Visual Studio x64 developer
# environment does):
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-wasapi-bindings.sh
#
# As scripts/gen-d3d11-bindings.sh: the installed SDK is the source, its
# version written into the generated files; every struct carries a layout
# assertion; the interfaces are declared in their C form; nothing is linked.
# The error codes are the header's own macros, evaluated by clang.

set -euo pipefail

export MSYS2_ARG_CONV_EXCL='//'
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd)"
out="$root/crates/audio/src/ffi"
kits="${WindowsSdkDir:-C:/Program Files (x86)/Windows Kits/10/}"
version="${WindowsSDKVersion:-10.0.26100.0}"
version="${version%\\}"
version="${version%/}"
inc="$kits/Include/$version"
[ -f "$inc/um/mmdeviceapi.h" ] || { echo "no SDK headers at $inc" >&2; exit 1; }
mkdir -p "$out"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/wasapi.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <ole2.h>
#include <mmreg.h>
#include <mmdeviceapi.h>
#include <audioclient.h>
#include <endpointvolume.h>
#include <propsys.h>
#include <audiopolicy.h>
#include <audioclientactivationparams.h>
EOF

# Interfaces and values reached only by pointer, never called or read here:
# opaque, so their own tables and unions are not generated. A property's
# value is read through the system's own declaration of it.
opaque=(
    'IStream|IStorage|IDispatch|IRecordInfo|ITypeInfo|IEnumSTATPROPSTG|IPropertySetStorage'
    'IAudioEndpointVolumeCallback'
    'IAudioSessionEvents|IAudioVolumeDuckNotification|ISimpleAudioVolume'
    'PROPVARIANT|tagPROPVARIANT|SAFEARRAY|tagSAFEARRAY|BLOB|tagBLOB'
    'DECIMAL|tagDEC|CY|tagCY|BSTR_BLOB|CLIPDATA|tagCLIPDATA|VERSIONEDSTREAM|tagVersionedStream'
)
opaque_args=()
for pattern in "${opaque[@]}"; do
    opaque_args+=(--opaque-type "$pattern" --allowlist-type "$pattern")
done

echo "generating wasapi bindings against SDK $version"
bindgen "$work/wasapi.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    --merge-extern-blocks \
    --wrap-unsafe-ops \
    --clang-macro-fallback \
    --raw-line "// Windows SDK $version." \
    `# The interfaces called: the enumeration and its devices, the one this` \
    `# side implements to hear of a new default, a device's audio client with` \
    `# its capture and render halves, its volume, and its property store.` \
    --allowlist-type 'IMMDevice(Enumerator|Collection)?|IMMNotificationClient' \
    --allowlist-type 'IAudio(Client|CaptureClient|RenderClient|EndpointVolume)|IPropertyStore|IUnknown'     `# The sessions an output holds, which say which processes play, and the`     `# one this side implements to hear of a new one; the activation of a`     `# process's capture, with the completion this side implements.`     --allowlist-type 'IAudioSession(Manager2|Enumerator|Control2|Notification)|AudioSessionState'     --allowlist-type 'IActivateAudioInterface(AsyncOperation|CompletionHandler)'     --allowlist-type 'AUDIOCLIENT_(ACTIVATION_PARAMS|PROCESS_LOOPBACK_PARAMS|ACTIVATION_TYPE)|PROCESS_LOOPBACK_MODE' \
    `# What the calls take: the format, a property's key, which flow and role.` \
    --allowlist-type 'WAVEFORMATEX|PROPERTYKEY|EDataFlow|ERole|AUDCLNT_SHAREMODE|_AUDCLNT_BUFFERFLAGS' \
    `# Flags and codes passed as plain integers.` \
    --allowlist-var 'AUDCLNT_STREAMFLAGS_(LOOPBACK|EVENTCALLBACK|AUTOCONVERTPCM|SRC_DEFAULT_QUALITY)' \
    --allowlist-var 'AUDCLNT_E_(DEVICE_INVALIDATED|DEVICE_IN_USE|SERVICE_NOT_RUNNING|RESOURCES_INVALIDATED)' \
    --allowlist-var 'DEVICE_STATE_ACTIVE|ENDPOINT_HARDWARE_SUPPORT_MUTE|WAVE_FORMAT_IEEE_FLOAT|STGM_READ' \
    --allowlist-var 'E_NOINTERFACE' \
    "${opaque_args[@]}" \
    -o "$out/wasapi.rs" \
    -- --target=x86_64-pc-windows-msvc

# Identifiers, transcribed mechanically from the header text as the video
# bindings' are: the headers declare them as external objects that only an
# import library defines, which nothing here links.
echo "generating wasapi identifiers"
"$py" - "$inc" "$out/wasapi_guids.rs" "$version" <<'PY'
import re
import sys

inc, dst, version = sys.argv[1], sys.argv[2], sys.argv[3]

def read(rel):
    return open(inc + "/" + rel, encoding="utf-8", errors="replace").read()

interfaces = {
    "um/unknwnbase.h": ["IUnknown"],
    "um/mmdeviceapi.h": ["IMMDeviceEnumerator", "IMMDevice", "IMMDeviceCollection",
                         "IMMNotificationClient", "IActivateAudioInterfaceCompletionHandler"],
    "um/audioclient.h": ["IAudioClient", "IAudioCaptureClient", "IAudioRenderClient"],
    "um/endpointvolume.h": ["IAudioEndpointVolume"],
    "um/propsys.h": ["IPropertyStore"],
    "um/audiopolicy.h": ["IAudioSessionManager2", "IAudioSessionControl2",
                         "IAudioSessionNotification"],
    "um/objidlbase.h": ["IAgileObject"],
}

lines = [
    "// Generated by scripts/gen-wasapi-bindings.sh. Do not edit.",
    "// Windows SDK %s." % version,
    "",
    "use super::wasapi::{GUID, PROPERTYKEY};",
    "",
]

def guid(d1, d2, d3, octets, indent):
    if len(octets) != 8:
        raise SystemExit("a guid has %d octets, expected 8" % len(octets))
    pad = " " * indent
    return [
        "%sGUID {" % pad,
        "%s    Data1: 0x%08x," % (pad, d1),
        "%s    Data2: 0x%04x," % (pad, d2),
        "%s    Data3: 0x%04x," % (pad, d3),
        "%s    Data4: [%s]," % (pad, ", ".join("0x%02x" % o for o in octets)),
        "%s}" % pad,
    ]

def from_text(h):
    h = h.replace("-", "")
    return int(h[0:8], 16), int(h[8:12], 16), int(h[12:16], 16), \
        [int(h[i:i + 2], 16) for i in range(16, 32, 2)]

count = 0
for header, names in interfaces.items():
    text = read(header)
    for name in names:
        # The base interface has no base of its own, so a brace follows it.
        m = re.search(r'MIDL_INTERFACE\("([0-9A-Fa-f-]{36})"\)\s*\n\s*%s\s*[:{]' % name, text)
        if not m:
            raise SystemExit("no identifier for %s in %s" % (name, header))
        body = guid(*from_text(m.group(1)), indent=0)
        lines.append("pub const IID_%s: GUID = %s" % (name, body[0]))
        lines.extend(body[1:-1])
        lines.append("};")
        count += 1

# The enumerator's class, declared as a class carrying its identifier.
text = read("um/mmdeviceapi.h")
m = re.search(r'DECLSPEC_UUID\("([0-9A-Fa-f-]{36})"\)\s*\n?\s*MMDeviceEnumerator\s*;', text)
if not m:
    raise SystemExit("no class identifier for MMDeviceEnumerator")
body = guid(*from_text(m.group(1)), indent=0)
lines.append("pub const CLSID_MMDeviceEnumerator: GUID = %s" % body[0])
lines.extend(body[1:-1])
lines.append("};")
count += 1

# The one property read: the name a person sees.
text = read("um/functiondiscoverykeys_devpkey.h")
m = re.search(r"DEFINE_PROPERTYKEY\(PKEY_Device_FriendlyName,\s*([^)]*)\)", text)
if not m:
    raise SystemExit("no PKEY_Device_FriendlyName")
parts = [int(p.strip(), 0) for p in m.group(1).split(",")]
if len(parts) != 12:
    raise SystemExit("PKEY_Device_FriendlyName has %d parts, expected 12" % len(parts))
lines.append("pub const PKEY_Device_FriendlyName: PROPERTYKEY = PROPERTYKEY {")
lines.append("    fmtid: GUID {")
lines.extend(guid(parts[0], parts[1], parts[2], parts[3:11], indent=4)[1:-1])
lines.append("    },")
lines.append("    pid: %d," % parts[11])
lines.append("};")
count += 1

if count != 17:
    raise SystemExit("%d identifiers matched, expected 17; the header layout changed" % count)

open(dst, "w", encoding="ascii", newline="\n").write("\n".join(lines) + "\n")
print("  %d identifiers" % count)
PY

rustfmt --edition 2024 "$out/wasapi.rs" "$out/wasapi_guids.rs"
