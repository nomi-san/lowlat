#!/usr/bin/env bash
# Regenerate the display driver's declarations from the Windows Driver Kit's
# headers: the user-mode driver framework at 2.25 and the indirect display
# class at 1.4, the oldest the driver's floor needs (docs/11-drivers.md 1.1).
#
# Run by hand on Windows, never from a build script: the generated file is
# committed so that no build machine needs a C toolchain. The kit comes from
# scripts/fetch-wdk.py (or LOWLAT_WDK_DIR); the installed SDK supplies the
# system headers beneath it. Needs LLVM's clang on the path:
#
#   cargo install bindgen-cli --version 0.72.1 --locked
#   scripts/gen-vdd-bindings.sh
#
# The class's header is C++ (it names structures without their tag), so the
# headers are read as C++ with the compiler's Microsoft extensions; every
# declaration they make is C-linked, which the absence of a mangled name in the
# output shows. The frameworks' calls are inline functions over a function
# table and are written by hand beside this file; what is generated is the
# tables, their indices, the entry types and the structures they take. The two
# minimum-version words are the driver's to define, so they are left out.

set -euo pipefail

export MSYS2_ARG_CONV_EXCL='*'
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

# Windows-form paths throughout: path conversion is off for the native tools.
root="$(cd "$(dirname "$0")/.." && pwd -W)"
out="$root/drivers/vdd/src/ffi"
kit="${LOWLAT_WDK_DIR:-$("$py" "$root/scripts/fetch-wdk.py")}"
kit="$(cygpath -m "$kit")/c"
sdks="${WindowsSdkDir:-C:/Program Files (x86)/Windows Kits/10/}"
version="${WindowsSDKVersion:-10.0.26100.0}"
version="${version%\\}"
version="${version%/}"
sdk="$(cygpath -m "$sdks")/Include/$version"
inc="$kit/Include"
[ -f "$inc/wdf/umdf/2.25/wdf.h" ] || { echo "no UMDF 2.25 headers under $inc" >&2; exit 1; }
[ -f "$inc/$version/um/iddcx/1.4/IddCx.h" ] || { echo "no IddCx 1.4 headers under $inc" >&2; exit 1; }
[ -f "$sdk/um/windows.h" ] || { echo "no SDK headers at $sdk" >&2; exit 1; }
kit_version="$(basename "$(dirname "$kit")")"
mkdir -p "$out"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

cat > "$work/vdd.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>
#define UMDF_VERSION_MAJOR 2
#define UMDF_VERSION_MINOR 25
#define WDF_STUB
#include <wdf.h>
#define IDDCX_VERSION_MAJOR 1
#define IDDCX_VERSION_MINOR 4
#define IDDCX_MINIMUM_VERSION_REQUIRED 4
#define IDD_STUB
#include <IddCx.h>
EOF

# The framework calls made, by their entry types; the table's index for each
# comes with the index enumeration.
wdf_calls='DRIVERCREATE|DEVICEINITSETPNPPOWEREVENTCALLBACKS|DEVICEINITSETFILEOBJECTCONFIG'
wdf_calls+='|DEVICECREATE|DEVICECREATEDEVICEINTERFACE|OBJECTGETTYPEDCONTEXTWORKER'
wdf_calls+='|OBJECTALLOCATECONTEXT|OBJECTDELETE|REQUESTCOMPLETE|REQUESTCOMPLETEWITHINFORMATION'
wdf_calls+='|REQUESTRETRIEVEINPUTBUFFER|REQUESTRETRIEVEOUTPUTBUFFER|REQUESTGETFILEOBJECT'
idd_calls='DEVICEINITCONFIG|DEVICEINITIALIZE|ADAPTERINITASYNC|MONITORCREATE|MONITORARRIVAL'
idd_calls+='|MONITORDEPARTURE|MONITORSETUPHARDWARECURSOR|MONITORQUERYHARDWARECURSOR'
idd_calls+='|SWAPCHAINSETDEVICE|SWAPCHAINRELEASEANDACQUIREBUFFER|SWAPCHAINFINISHEDPROCESSINGFRAME'
idd_calls+='|SWAPCHAINGETDIRTYRECTS|GETVERSION|ADAPTERSETRENDERADAPTER|REPORTCRITICALERROR'

echo "generating vdd bindings against WDK $kit_version, SDK $version"
bindgen "$(cygpath -m "$work")/vdd.h" \
    --no-doc-comments \
    --no-prepend-enum-name \
    `# Zero is every structure's empty value here; the driver fills fields.` \
    --with-derive-default \
    --merge-extern-blocks \
    --wrap-unsafe-ops \
    --clang-macro-fallback \
    --raw-line "// Windows Driver Kit $kit_version (UMDF 2.25, IddCx 1.4), Windows SDK $version." \
    `# The tables, their indices, and what the frameworks keep about the` \
    `# version this driver asked for against the one that loaded it.` \
    --allowlist-type '_?WDFFUNCENUM|_?IDDFUNCENUM' \
    --allowlist-var 'WdfFunctions_02025|WdfDriverGlobals|WdfClientVersionHigherThanFramework|WdfFunctionCount' \
    --allowlist-var 'IddFunctions|IddDriverGlobals|IddClientVersionHigherThanFramework|IddFunctionCount' \
    --allowlist-var 'IddStructureCount|IddStructures' \
    --blocklist-item 'WdfMinimumVersionRequired|IddMinimumVersionRequired' \
    `# The calls' entry types, and everything they take, transitively.` \
    --allowlist-type "PFN_WDF($wdf_calls)" \
    --allowlist-type "PFN_IDDCX($idd_calls)" \
    `# What the driver hands the frameworks: its own callbacks' configuration.` \
    --allowlist-type '_?WDF_DRIVER_CONFIG|_?WDF_PNPPOWER_EVENT_CALLBACKS|_?WDF_FILEOBJECT_CONFIG' \
    --allowlist-type '_?WDF_OBJECT_ATTRIBUTES|_?WDF_OBJECT_CONTEXT_TYPE_INFO|IDD_CX_CLIENT_CONFIG' \
    `# The direct graphics objects the class names travel as pointers alone.` \
    --opaque-type 'IDXGIDevice|IDXGIResource' --allowlist-type 'IDXGIDevice|IDXGIResource' \
    -o "$out/umdf.rs" \
    -- --target=x86_64-pc-windows-msvc -x c++ -std=c++17 -fms-compatibility -fms-extensions \
    -Wno-nonportable-include-path -Wno-microsoft-enum-forward-reference -Wno-pragma-pack -Wno-switch \
    -I "$inc/wdf/umdf/2.25" -I "$inc/$version/um/iddcx/1.4" -I "$inc/$version/um" -I "$inc/$version/shared" \
    -I "$sdk/um" -I "$sdk/shared" -I "$sdk/ucrt"

# The output must carry no C++ name: every symbol is the C one the stub
# libraries define.
if grep -q 'link_name' "$out/umdf.rs"; then
    echo "a declaration came out C++-mangled; the headers' linkage changed" >&2
    exit 1
fi

# The status values, in a pass of their own: clang evaluates a macro for the
# generator only when the headers are read as C, and the system's status
# header names a type the windowing headers leave out.
cat > "$work/status.h" <<'EOF'
#define WIN32_LEAN_AND_MEAN
#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
typedef LONG NTSTATUS;
#include <ntstatus.h>
#include <dxgi.h>
EOF

echo "generating vdd status values"
bindgen "$(cygpath -m "$work")/status.h" \
    --no-doc-comments \
    --clang-macro-fallback \
    --default-macro-constant-type signed \
    --raw-line "// Windows SDK $version." \
    --allowlist-var 'STATUS_(SUCCESS|UNSUCCESSFUL|INVALID_PARAMETER|BUFFER_TOO_SMALL|INSUFFICIENT_RESOURCES)' \
    --allowlist-var 'STATUS_(INVALID_DEVICE_REQUEST|ACCESS_DENIED|NOT_SUPPORTED|NO_SUCH_DEVICE|INVALID_DEVICE_STATE|DEVICE_BUSY)' \
    --allowlist-var 'STATUS_GRAPHICS_(INDIRECT_DISPLAY_ABANDON_SWAPCHAIN|INDIRECT_DISPLAY_DEVICE_STOPPED|PATH_NOT_IN_TOPOLOGY)' \
    --allowlist-var 'E_PENDING|DXGI_ERROR_ACCESS_LOST' \
    -o "$out/status.rs" \
    -- --target=x86_64-pc-windows-msvc -I "$sdk/um" -I "$sdk/shared" -I "$sdk/ucrt"

count="$(grep -c '^pub const' "$out/status.rs")"
[ "$count" = 16 ] || { echo "$count status values, expected 16; the headers changed" >&2; exit 1; }

rustfmt --edition 2024 "$out/umdf.rs" "$out/status.rs"
echo "  $(wc -l < "$out/umdf.rs") lines, $count status values"
