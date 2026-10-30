#!/usr/bin/env bash
# Build SDL3 from its release, static, into a prefix: for the client demo's
# Linux build where the system packages no SDL3, as on the build workflows'
# runner, and for its macOS build. The demo then links it through pkg-config:
#
#   ./scripts/build-sdl3.sh "$RUNNER_TEMP/sdl3"
#   PKG_CONFIG_PATH="$RUNNER_TEMP/sdl3/lib/pkgconfig" make -C examples/client client
#
# The version is the one the demo's Windows build takes from vcpkg
# (examples/client/vcpkg.json). Only what the demo uses is built, sound and
# the pads. On Linux their backends -- PulseAudio, PipeWire, ALSA, and udev
# for the pads' raw nodes -- are loaded at run time as a distribution's
# package loads them, so a demo linked with it needs none of them installed
# to start. On macOS they are the system's own frameworks, and the library
# holds both architectures, so one prefix serves a demo built for either.
#
# Needs cmake, curl and, on Linux, those backends' development headers, and
# fails when one of them was not found rather than building an SDL without it.
set -eu

prefix=${1:?the prefix to install into}
version=3.4.18
sha512=12fcb5592016b27922b08d1580d75f5f15c72e67cf04415e2bd824dc7563ac940c25bbe368b03d0a755f5832803049467e3d0ee6784ed3684792bc4d7741af50

# The demo draws through its toolkit, so SDL makes no windows, which its
# Linux build refuses unless told the console build is meant.
case "$(uname -s)" in
Darwin)
    sha512sum() { shasum -a 512 "$@"; }
    jobs=$(sysctl -n hw.ncpu)
    platform=(-DCMAKE_OSX_ARCHITECTURES="arm64;x86_64")
    ;;
*)
    jobs=$(nproc)
    platform=(-DSDL_UNIX_CONSOLE_BUILD=ON)
    ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/sdl.tar.gz" \
    "https://github.com/libsdl-org/SDL/archive/refs/tags/release-$version.tar.gz"
echo "$sha512  $work/sdl.tar.gz" | sha512sum -c --quiet -
tar -xzf "$work/sdl.tar.gz" -C "$work"
# A static library linked into a position-independent program must be
# position-independent itself, which SDL's static build is not by default.
cmake -S "$work/SDL-release-$version" -B "$work/build" -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$prefix" -DCMAKE_INSTALL_LIBDIR=lib \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DSDL_SHARED=OFF -DSDL_STATIC=ON -DSDL_TEST_LIBRARY=OFF -DSDL_TESTS=OFF -DSDL_EXAMPLES=OFF \
    -DSDL_VIDEO=OFF -DSDL_HIDAPI_LIBUSB=OFF "${platform[@]}"
cmake --build "$work/build" -j "$jobs"
cmake --install "$work/build"

# Each of Linux's backends is loaded by its library's name; one whose headers
# were missing leaves no name behind.
if [ "$(uname -s)" != Darwin ]; then
    for lib in libpulse.so.0 libpipewire-0.3.so.0 libasound.so.2 libudev.so.1; do
        if ! strings "$prefix/lib/libSDL3.a" | grep -qx "$lib"; then
            echo "build-sdl3: built without $lib; install its development headers" >&2
            exit 1
        fi
    done
fi
echo "build-sdl3: SDL $version in $prefix"
