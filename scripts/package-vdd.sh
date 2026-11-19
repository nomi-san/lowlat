#!/usr/bin/env bash
# Package the display driver for installation: the library built in release,
# under its brand's name, the setup information its build wrote with the
# brand's names, stamped with the driver's version and the user-mode
# framework's, the package's catalogue, and the library and the catalogue
# signed with the certificate named by its thumbprint (docs/11-drivers.md
# section 1).
#
#   scripts/package-vdd.sh [<thumbprint>]
#
# The brand is the build's: lowlat's own, or the file LOWLAT_VDD_BRAND names
# (docs/11-drivers.md section 1.10). In development the certificate is the
# machine's own test certificate (scripts/vdd-test-cert.ps1); a release is
# signed from CI's build instead. Without a thumbprint the package is
# catalogued and left unsigned, for a signature made elsewhere -- a company's
# own certificate, on its own token: the library and the catalogue, in either
# order, since the catalogue records the library's digest without its
# signature. The output is target/vdd-package, with the names the install
# script reads in package.json. Needs the driver kit (scripts/fetch-wdk.py)
# and the SDK's signtool.

set -euo pipefail

# Windows-form paths throughout: path conversion is off for the native tools,
# whose switches start with a slash.
export MSYS2_ARG_CONV_EXCL='*'
thumbprint="${1:-}"
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd -W)"
kit="$(cygpath -m "$("$py" "$root/scripts/fetch-wdk.py")")/c"
sdkbin="$(cygpath -m "${WindowsSdkDir:-C:/Program Files (x86)/Windows Kits/10/}")/bin/10.0.26100.0/x64"
out="$root/target/vdd-package"
# The kit's tools read a path with forward slashes as a relative one.
win() { cygpath -w "$1"; }
for tool in "$kit/bin/10.0.26100.0/x64/stampinf.exe" "$kit/bin/10.0.26100.0/x86/Inf2Cat.exe" \
    "$kit/tools/10.0.26100.0/x64/infverif.exe" "$sdkbin/signtool.exe"; do
    [ -f "$tool" ] || { echo "missing $tool" >&2; exit 1; }
done

# The driver built, and where: the library, and the folder its build wrote
# the setup information and the names into, from cargo's own messages.
built="$(cargo build --manifest-path "$root/Cargo.toml" -p lowlat-vdd --release \
    --message-format=json-render-diagnostics | "$py" -c '
import json, pathlib, re, sys
library = made = None
for line in sys.stdin:
    message = json.loads(line)
    if not re.search("(^|#)lowlat-vdd[@ ]", message.get("package_id", "")):
        continue
    if message["reason"] == "build-script-executed":
        made = pathlib.PureWindowsPath(message["out_dir"], "package")
    elif message["reason"] == "compiler-artifact" and "cdylib" in message["target"]["kind"]:
        library = next(f for f in message["filenames"] if f.endswith(".dll"))
if not (library and made):
    sys.exit("the build named no library or no package folder")
print(pathlib.PureWindowsPath(library).as_posix())
print(made.as_posix())
' | tr -d '\r')"
library="$(sed -n 1p <<< "$built")"
made="$(sed -n 2p <<< "$built")"
read -r dll inf cat < <("$py" -c '
import json, sys
names = json.load(open(sys.argv[1]))
print(names["library"], names["inf"], names["catalog"])
' "$made/package.json" | tr -d '\r')

rm -rf "$out"
mkdir -p "$out"
cp "$library" "$out/$dll"
cp "$made/$inf" "$made/package.json" "$out/"

# A version that grows with every package, so an update replaces the driver.
# Dated in UTC as the version is: stampinf's own `*` is the local date, which
# east of Greenwich is a day ahead of UTC for hours after local midnight, and
# infverif refuses a date still to come.
version="0.1.$(date -u +%y%j).$(date -u +%H%M)"
"$kit/bin/10.0.26100.0/x64/stampinf.exe" -f "$(win "$out/$inf")" -a amd64 \
    -d "$(date -u +%m/%d/%Y)" -v "$version" -u 2.25.0
"$kit/tools/10.0.26100.0/x64/infverif.exe" /v "$(win "$out/$inf")"

sign() {
    [ -n "$thumbprint" ] || return 0
    "$sdkbin/signtool.exe" sign /q /fd SHA256 /s My /sha1 "$thumbprint" "$(win "$1")"
}
# The library first: the catalogue records its digest, which leaves its own
# signature out.
sign "$out/$dll"
"$kit/bin/10.0.26100.0/x86/Inf2Cat.exe" /driver:"$(win "$out")" \
    /os:10_19H1_X64,10_VB_X64,10_CO_X64,10_NI_X64,10_GE_X64
sign "$out/$cat"
if [ -z "$thumbprint" ]; then
    echo "packaged $version of $dll, $inf and $cat in $out, unsigned: sign $dll and $cat"
    exit 0
fi
"$sdkbin/signtool.exe" verify /q /pa /c "$(win "$out/$cat")" "$(win "$out/$dll")"
echo "packaged $version of $dll, $inf and $cat in $out"
