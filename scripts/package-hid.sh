#!/usr/bin/env bash
# Package the input driver for installation: the library built in release,
# a copy of its setup information stamped with the driver's version and the
# user-mode framework's, the package's catalogue, and the library and the
# catalogue signed with the certificate named by its thumbprint
# (docs/11-drivers.md section 2).
#
#   scripts/package-hid.sh <thumbprint>
#
# In development the certificate is the machine's own test certificate
# (scripts/vdd-test-cert.ps1); a release is signed from CI's build instead.
# The output is target/hid-package. Needs the driver kit (scripts/fetch-wdk.py)
# and the SDK's signtool.

set -euo pipefail

# Windows-form paths throughout: path conversion is off for the native tools,
# whose switches start with a slash.
export MSYS2_ARG_CONV_EXCL='*'
thumbprint="${1:?usage: scripts/package-hid.sh <certificate thumbprint>}"
py=""
for candidate in python3 python; do
    if "$candidate" -c '' 2>/dev/null; then py="$candidate"; break; fi
done
[ -n "$py" ] || { echo "no python" >&2; exit 1; }

root="$(cd "$(dirname "$0")/.." && pwd -W)"
kit="$(cygpath -m "$("$py" "$root/scripts/fetch-wdk.py")")/c"
sdkbin="$(cygpath -m "${WindowsSdkDir:-C:/Program Files (x86)/Windows Kits/10/}")/bin/10.0.26100.0/x64"
out="$root/target/hid-package"
# The kit's tools read a path with forward slashes as a relative one.
win() { cygpath -w "$1"; }
for tool in "$kit/bin/10.0.26100.0/x64/stampinf.exe" "$kit/bin/10.0.26100.0/x86/Inf2Cat.exe" \
    "$kit/tools/10.0.26100.0/x64/infverif.exe" "$sdkbin/signtool.exe"; do
    [ -f "$tool" ] || { echo "missing $tool" >&2; exit 1; }
done

cargo build --manifest-path "$root/Cargo.toml" -p lowlat-hid --release
rm -rf "$out"
mkdir -p "$out"
cp "$root/target/release/lowlat_hid.dll" "$out/"
# The setup information in the system's own line endings.
"$py" -c 'import sys; d = open(sys.argv[1], "rb").read().replace(b"\r\n", b"\n"); open(sys.argv[2], "wb").write(d.replace(b"\n", b"\r\n"))' \
    "$root/crates/hid/lowlat-hid.inf" "$out/lowlat-hid.inf"

# A version that grows with every package, so an update replaces the driver.
# Dated in UTC as the version is: stampinf's own `*` is the local date, which
# east of Greenwich is a day ahead of UTC for hours after local midnight, and
# infverif refuses a date still to come.
version="0.1.$(date -u +%y%j).$(date -u +%H%M)"
"$kit/bin/10.0.26100.0/x64/stampinf.exe" -f "$(win "$out/lowlat-hid.inf")" -a amd64 \
    -d "$(date -u +%m/%d/%Y)" -v "$version" -u 2.25.0
"$kit/tools/10.0.26100.0/x64/infverif.exe" /v "$(win "$out/lowlat-hid.inf")"

sign() {
    "$sdkbin/signtool.exe" sign /q /fd SHA256 /s My /sha1 "$thumbprint" "$(win "$1")"
}
# The library first: the catalogue records its digest, which leaves its own
# signature out.
sign "$out/lowlat_hid.dll"
"$kit/bin/10.0.26100.0/x86/Inf2Cat.exe" /driver:"$(win "$out")" \
    /os:10_19H1_X64,10_VB_X64,10_CO_X64,10_NI_X64,10_GE_X64
sign "$out/lowlat-hid.cat"
"$sdkbin/signtool.exe" verify /q /pa /c "$(win "$out/lowlat-hid.cat")" "$(win "$out/lowlat_hid.dll")"
echo "packaged $version in $out"
