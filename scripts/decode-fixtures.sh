#!/usr/bin/env bash
# Make the decoder's syntax fixtures: small streams from other encoders,
# each exercising one part of the syntax, with reference checksums from an
# independent decoder. Runs the system ffmpeg as a separate process; the
# fixtures are data and nothing of that process reaches the build.
#
#   scripts/decode-fixtures.sh [out-dir]
#
# Small pictures keep the set cheap to commit; a device's floor on the coded
# size is a compatibility fact the hardware test reports, not a reason to
# grow them.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/crates/decode/tests/data/fixtures}"
mkdir -p "$out"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

frames=24
src() {
    # A moving test pattern; -r fixes the timestamps the encoder paces by.
    echo "-f lavfi -i testsrc2=size=$1:rate=30 -frames:v $frames -r 30"
}

make_fixture() {
    local name=$1 codec=$2 pix=$3 size=$4
    shift 4
    local ext=$codec
    # Every encoder is asked for access unit delimiters, which is what the
    # splitter cuts at.
    # shellcheck disable=SC2046
    ffmpeg -v error -y $(src "$size") -pix_fmt "$pix" "$@" -f "$codec" "$tmp/$name.$ext"
    python3 "$root/scripts/decode-clip.py" annexb "$tmp/$name.$ext" "$codec" "$out/$name.bin"
    local raw=nv12
    case "$pix" in *10*) raw=p010le ;; esac
    python3 "$root/scripts/decode-clip.py" sums "$out/$name.bin" "$codec" "$raw" "$out/$name.sums"
}

# H.264. x264 codes progressive streams without B pictures under order
# count type 2 and everything else under type 0, so both types appear.
make_fixture h264-ipp-cabac       h264 yuv420p 128x128 -c:v libx264 -preset fast -bf 0 -refs 1 -x264-params "cabac=1:keyint=12:aud=1"
make_fixture h264-ipp-cavlc       h264 yuv420p 128x128 -c:v libx264 -preset fast -bf 0 -refs 3 -x264-params "cabac=0:keyint=12:aud=1"
make_fixture h264-bframes         h264 yuv420p 128x128 -c:v libx264 -preset medium -bf 3 -refs 4 -x264-params "b-pyramid=normal:weightp=2:weightb=1:8x8dct=1:keyint=12:aud=1"
make_fixture h264-slices          h264 yuv420p 128x128 -c:v libx264 -preset fast -bf 2 -x264-params "slices=4:keyint=12:aud=1"
make_fixture h264-scaling         h264 yuv420p 128x128 -c:v libx264 -preset medium -bf 2 -x264-params "cqm=jvt:8x8dct=1:keyint=12:aud=1"
make_fixture h264-mbaff           h264 yuv420p 128x128 -c:v libx264 -preset medium -bf 2 -flags +ildct+ilme -x264-params "interlaced=1:keyint=12:aud=1"
# The vendor encoder's shape: what an established host sends.
make_fixture h264-nvenc-ll        h264 yuv420p 256x256 -c:v h264_nvenc -preset p1 -tune ll -rc cbr -b:v 1M -bf 0 -g 12 -aud 1
make_fixture h264-nvenc-bframes   h264 yuv420p 256x256 -c:v h264_nvenc -preset p4 -bf 2 -b_ref_mode middle -g 12 -aud 1

# HEVC.
make_fixture hevc-ipp             hevc yuv420p   128x128 -c:v libx265 -preset fast -x265-params "bframes=0:keyint=12:aud=1:log-level=error"
make_fixture hevc-bframes         hevc yuv420p   128x128 -c:v libx265 -preset medium -x265-params "bframes=4:b-pyramid=1:ref=4:weightb=1:keyint=12:aud=1:log-level=error"
make_fixture hevc-slices          hevc yuv420p   256x256 -c:v libx265 -preset fast -x265-params "slices=2:bframes=2:keyint=12:aud=1:log-level=error"
make_fixture hevc-scaling         hevc yuv420p   128x128 -c:v libx265 -preset medium -x265-params "scaling-list=default:bframes=2:keyint=12:aud=1:log-level=error"
make_fixture hevc-main10          hevc yuv420p10le 256x256 -c:v libx265 -preset fast -x265-params "bframes=2:keyint=12:aud=1:log-level=error"
make_fixture hevc-nvenc-ll        hevc yuv420p   256x256 -c:v hevc_nvenc -preset p1 -tune ll -rc cbr -b:v 1M -bf 0 -g 12 -aud 1
make_fixture hevc-nvenc-main10    hevc p010le    256x256 -c:v hevc_nvenc -preset p1 -tune ll -rc cbr -b:v 2M -bf 0 -g 12 -profile:v main10 -aud 1

ls -l "$out"
