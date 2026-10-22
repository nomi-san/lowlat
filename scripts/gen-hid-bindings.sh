#!/usr/bin/env bash
# Regenerate the input driver's declarations: the user-mode driver framework
# at 2.25 from the Windows Driver Kit's headers, and the virtual HID framework
# from the installed SDK's, whose header the kit does not carry beside its
# library (docs/11-drivers.md 2.1).
#
# Run by hand on Windows, never from a build script: the generated files are
# committed so that no build machine needs a C toolchain. The kit comes from
# scripts/fetch-wdk.py (or LOWLAT_WDK_DIR); the installed SDK supplies the
# system headers beneath it and the virtual HID framework's own. Needs LLVM's
# clang on the path:
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-hid-bindings.sh
#
# The framework's calls are inline functions over a function table and are
# written by hand beside this file; what is generated is the table, its
# indices, the entry types and the structures they take. The virtual HID
# framework's calls are plain functions its library exports, generated as
# they are. The framework's minimum-version word is the driver's to define,
# so it is left out. The display driver's declarations are a set of their own
# (scripts/gen-vdd-bindings.sh), so neither driver changes for the other.

set -euo pipefail

export MSYS2_ARG_CONV_EXCL='*'
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

# Windows-form paths throughout: path conversion is off for the native tools.
root="$(cd "$(dirname "$0")/.." && pwd -W)"
out="$root/crates/hid/src/ffi"
kit="${LOWLAT_WDK_DIR:-$("$py" "$root/scripts/fetch-wdk.py")}"
kit="$(cygpath -m "$kit")/c"
sdks="${WindowsSdkDir:-C:/Program Files (x86)/Windows Kits/10/}"
version="${WindowsSDKVersion:-10.0.26100.0}"
version="${version%\\}"
version="${version%/}"
sdk="$(cygpath -m "$sdks")/Include/$version"
inc="$kit/Include"
[ -f "$inc/wdf/umdf/2.25/wdf.h" ] || { echo "no UMDF 2.25 headers under $inc" >&2; exit 1; }
[ -f "$sdk/um/windows.h" ] || { echo "no SDK headers at $sdk" >&2; exit 1; }
[ -f "$sdk/shared/vhf.h" ] || { echo "no vhf.h under $sdk/shared" >&2; exit 1; }
kit_version="$(basename "$(dirname "$kit")")"
mkdir -p "$out"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/hid.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#define UMDF_VERSION_MAJOR 2
#define UMDF_VERSION_MINOR 25
#define WDF_STUB
#include <wdf.h>
#include <vhf.h>
EOF

# The framework calls made, by their entry types; the table's index for each
# comes with the index enumeration.
wdf_calls='DRIVERCREATE|FDOINITSETFILTER|DEVICEINITSETFILEOBJECTCONFIG|DEVICECREATE'
wdf_calls+='|DEVICECREATEDEVICEINTERFACE|IOQUEUECREATE|IOQUEUERETRIEVEREQUESTBYFILEOBJECT'
wdf_calls+='|REQUESTFORWARDTOIOQUEUE|REQUESTCOMPLETE|REQUESTCOMPLETEWITHINFORMATION'
wdf_calls+='|REQUESTRETRIEVEINPUTBUFFER|REQUESTRETRIEVEOUTPUTBUFFER|REQUESTGETFILEOBJECT'
wdf_calls+='|IOTARGETCREATE|IOTARGETOPEN|IOTARGETWDMGETTARGETFILEHANDLE|OBJECTDELETE'
wdf_calls+='|OBJECTGETTYPEDCONTEXTWORKER'

echo "generating hid bindings against WDK $kit_version, SDK $version"
bindgen "$(cygpath -m "$work")/hid.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    `# Zero is every structure's empty value here; the driver fills fields.` \
    --with-derive-default \
    --merge-extern-blocks \
    --wrap-unsafe-ops \
    --clang-macro-fallback \
    --raw-line "// Windows Driver Kit $kit_version (UMDF 2.25), Windows SDK $version (vhf.h)." \
    `# The table, its indices, and what the framework keeps about the version` \
    `# this driver asked for against the one that loaded it.` \
    --allowlist-type '_?WDFFUNCENUM' \
    --allowlist-var 'WdfFunctions_02025|WdfDriverGlobals|WdfClientVersionHigherThanFramework|WdfFunctionCount' \
    --blocklist-item 'WdfMinimumVersionRequired' \
    `# The calls' entry types, and everything they take, transitively.` \
    --allowlist-type "PFN_WDF($wdf_calls)" \
    `# What the driver hands the framework: its own callbacks' configuration.` \
    --allowlist-type '_?WDF_DRIVER_CONFIG|_?WDF_FILEOBJECT_CONFIG|_?WDF_OBJECT_ATTRIBUTES' \
    --allowlist-type '_?WDF_OBJECT_CONTEXT_TYPE_INFO' \
    --allowlist-type '_?WDF_IO_QUEUE_CONFIG|_?WDF_IO_TARGET_OPEN_PARAMS' \
    `# The virtual HID framework: its configuration, its packet and its calls.` \
    --allowlist-type '_?VHF_CONFIG|_?HID_XFER_PACKET' \
    --allowlist-function 'Vhf(Create|Start|Delete|ReadReportSubmit|AsyncOperationComplete)' \
    -o "$out/umdf.rs" \
    -- --target=x86_64-pc-windows-msvc -x c++ -std=c++17 -fms-compatibility -fms-extensions \
    -Wno-nonportable-include-path -Wno-microsoft-enum-forward-reference -Wno-pragma-pack -Wno-switch \
    -I "$inc/wdf/umdf/2.25" -I "$inc/$version/um" -I "$inc/$version/shared" \
    -I "$sdk/um" -I "$sdk/shared" -I "$sdk/ucrt"

# The output must carry no C++ name: every symbol is the C one the stub and
# the framework's library define.
if grep -q 'link_name' "$out/umdf.rs"; then
    echo "a declaration came out C++-mangled; the headers' linkage changed" >&2
    exit 1
fi
for call in VhfCreate VhfStart VhfDelete VhfReadReportSubmit VhfAsyncOperationComplete; do
    grep -q "pub fn $call(" "$out/umdf.rs" || { echo "no $call in the output" >&2; exit 1; }
done

# The status values, in a pass of their own: clang evaluates a macro for the
# generator only when the headers are read as C.
cat > "$work/status.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
typedef LONG NTSTATUS;
#include <ntstatus.h>
EOF

echo "generating hid status values"
bindgen "$(cygpath -m "$work")/status.h" \
    --no-doc-comments \
    --clang-macro-fallback \
    --default-macro-constant-type signed \
    --raw-line "// Windows SDK $version." \
    --allowlist-var 'STATUS_(SUCCESS|UNSUCCESSFUL|INVALID_PARAMETER|BUFFER_TOO_SMALL|INSUFFICIENT_RESOURCES)' \
    --allowlist-var 'STATUS_(INVALID_DEVICE_REQUEST|NOT_SUPPORTED|INVALID_DEVICE_STATE|DEVICE_BUSY)' \
    --allowlist-var 'STATUS_(CANCELLED|DEVICE_REMOVED|NO_MORE_ENTRIES|PENDING)' \
    -o "$out/status.rs" \
    -- --target=x86_64-pc-windows-msvc -I "$sdk/um" -I "$sdk/shared" -I "$sdk/ucrt"

count="$(grep -c '^pub const' "$out/status.rs")"
[ "$count" = 13 ] || { echo "$count status values, expected 13; the headers changed" >&2; exit 1; }

rustfmt --edition 2024 "$out/umdf.rs" "$out/status.rs"
echo "  $(wc -l < "$out/umdf.rs") lines, $count status values"
