#!/usr/bin/env bash
# The client's macOS zip (docs/impl-plan-macos.md M1.5): the client half as one universal static
# library, arm64 and x86_64 joined, with the header. Every check reads the copies that are zipped:
# both architectures in the library; no member built for a system later than the oldest the
# client supports; no host entry point; and the C test program linked to it three ways -- as C,
# and as C and as C++ with thin link-time optimisation and dead-stripping, as an application's
# release build links it -- each for that oldest system with the linker's warnings as errors,
# and run, x86_64's under the system's translation layer. Their lines are the zip's FEATURES, and
# the zip, unpacked, must hold exactly what was checked.
#
#   scripts/package-client-macos.sh [OUT_DIR] [NAME]
#
# Needs cargo with both Apple targets, Xcode's command-line tools, and the translation layer for
# the x86_64 runs. The floor is the workspace's own (.cargo/config.toml); a builder's later one
# fails the check rather than shipping.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/target/dist-client}
name=${2:-lowlat-sdk-macos-universal-client-$(git -C "$root" rev-parse --short=7 HEAD)}
manifest=$root/Cargo.toml
target=${CARGO_TARGET_DIR:-$root/target}
floor=11.0
archs="arm64 x86_64"

step() { echo "== $*"; }
fail() {
    echo "package: $*" >&2
    exit 1
}
triple() { if [ "$1" = arm64 ]; then echo aarch64-apple-darwin; else echo "$1-apple-darwin"; fi; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
stage=$out/$name
rm -rf "$stage" "$stage.zip"
mkdir -p "$stage/lib" "$stage/include"

# Each architecture's library lands where any other build of it would, so it is copied out the
# moment it is made.
for arch in $archs; do
    step "the client half for $arch, static"
    cargo rustc --release -p lowlat-sdk --no-default-features --features client \
        --target "$(triple "$arch")" --crate-type staticlib --manifest-path "$manifest"
    cp "$target/$(triple "$arch")/release/liblowlat.a" "$work/liblowlat-$arch.a"
done
step "the two joined"
lipo -create -output "$stage/lib/liblowlat.a" "$work/liblowlat-arm64.a" "$work/liblowlat-x86_64.a"
cp "$root/include/lowlat.h" "$stage/include/"
cp "$root/LICENSE" "$root/docs/06-api.md" "$root/docs/10-client.md" "$stage/"
lib=$stage/lib/liblowlat.a

for arch in $archs; do
    step "$arch: present, every member for macOS $floor or earlier, no host entry point"
    lipo "$lib" -verify_arch "$arch" || fail "the library holds no $arch"
    thin=$work/thin-$arch.a
    lipo "$lib" -thin "$arch" -output "$thin"
    # A member's floor is its build version's minos, or its older load command's version.
    otool -l "$thin" | awk '
        /cmd LC_BUILD_VERSION|cmd LC_VERSION_MIN_MACOSX/ { want = 1; next }
        want && ($1 == "minos" || $1 == "version") { print $2; want = 0 }' > "$work/floors-$arch"
    echo "$(ar -t "$thin" | wc -l | tr -d ' ') members; floors: $(sort "$work/floors-$arch" | uniq -c | xargs)"
    later=$(sort -u "$work/floors-$arch" | awk -v floor="$floor" '
        function value(v, part) { split(v, part, "."); return part[1] * 10000 + part[2] * 100 + part[3] }
        value($1) > value(floor)' | xargs)
    [ -z "$later" ] || fail "$arch members built for macOS $later"
    # The runtime's own members carry bitcode from a later LLVM than Xcode's, which nm's default
    # reader stops on; their symbols are read from the machine code.
    if ! symbols=$(nm --no-llvm-bc -gU "$thin" 2> "$work/nm"); then
        cat "$work/nm" >&2
        fail "nm could not read the $arch library"
    fi
    if echo "$symbols" | grep -E ' _lowlat_host_'; then
        fail "$arch defines a host entry point"
    fi
    echo "$(echo "$symbols" | grep -c ' T _lowlat_') lowlat_ entry points"
done

: > "$stage/FEATURES"
for arch in $archs; do
    for way in c c-lto c++-lto; do
        step "$arch: the C test program linked as $way and run"
        case $way in
        c) compile="cc -std=c11" optimise="" ;;
        c-lto) compile="cc -std=c11" optimise="-flto=thin -Wl,-dead_strip" ;;
        c++-lto) compile="c++ -std=c++17 -x c++" optimise="-flto=thin -Wl,-dead_strip" ;;
        esac
        program=$work/static-$arch-$way
        # Word-split on purpose: each holds whole options.
        # shellcheck disable=SC2086
        $compile -arch "$arch" -Wall -Wextra -Werror -mmacosx-version-min="$floor" \
            -I "$stage/include" "$root/crates/sdk/tests/c/static.c" -x none "$lib" \
            -Wl,-fatal_warnings $optimise -o "$program" || fail "$arch did not link as $way"
        # Its deliberate panic speaks on the error stream, kept unless the program fails.
        if ! line=$(arch "-$arch" "$program" 2> "$work/stderr"); then
            cat "$work/stderr" >&2
            fail "$arch as $way did not run: $line"
        fi
        echo "$line"
    done
    echo "$line" | sed -E "s/^static: version ([0-9.]+) features (0x[0-9a-f]+),.*/$arch abi \1 features \2/" \
        >> "$stage/FEATURES"
done
cat "$stage/FEATURES"
if grep -qv ' features 0x2$' "$stage/FEATURES"; then
    fail "the library is not the client half alone"
fi

step "the zip"
(cd "$out" && ditto -c -k --keepParent "$name" "$name.zip")
mkdir "$work/unpacked"
ditto -x -k "$stage.zip" "$work/unpacked"
diff -r "$stage" "$work/unpacked/$name" || fail "the zip does not hold what was checked"
find "$stage" -type f -exec ls -l {} + | awk '{ print $5, $NF }'
ls -l "$stage.zip" | awk '{ print $5, $NF }'
