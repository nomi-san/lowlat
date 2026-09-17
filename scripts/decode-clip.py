#!/usr/bin/env python3
"""Make and check the decoder's committed clips.

A clip is a file of access units, each prefixed by its length as a
little-endian 32-bit word, cut from a stream dump and its index of unit
lengths (`dump_what_a_guest_receives` writes both). Beside it sits a list of
per-picture plane checksums produced by an independent decoder run as a
separate process, so the decoder under test cannot bless its own output.

    scripts/decode-clip.py cut    <dump.bin> <dump.idx> <units> <clip.bin>
    scripts/decode-clip.py annexb <stream.h264|.hevc> h264|hevc <clip.bin>
    scripts/decode-clip.py sums   <clip.bin> h264|hevc nv12|p010le <clip.sums>

`annexb` splits a byte stream at its access unit delimiters (which the
encoder must have been told to write), so a fixture made by another encoder
takes the same form as a dump of ours.

A sums line is `picture y_crc uv_crc` in decimal, one per decoded picture in
output order.
"""

import struct
import subprocess
import sys
import zlib


def units(path):
    data = open(path, "rb").read()
    at = 0
    while at < len(data):
        (length,) = struct.unpack_from("<I", data, at)
        at += 4
        yield data[at : at + length]
        at += length


def nals(data):
    """(offset of the start code, offset of the unit's first byte) pairs."""
    at = 0
    while True:
        i = data.find(b"\x00\x00\x01", at)
        if i < 0:
            return
        yield i, i + 3
        at = i + 3


def annexb(path, codec, out):
    data = open(path, "rb").read()
    starts = []
    for code, first in nals(data):
        unit_type = (data[first] & 0x1F) if codec == "h264" else ((data[first] >> 1) & 0x3F)
        if (codec == "h264" and unit_type == 9) or (codec == "hevc" and unit_type == 35):
            # A four-byte start code begins one byte earlier.
            starts.append(code - 1 if code > 0 and data[code - 1] == 0 else code)
    if not starts:
        sys.exit("%s: no access unit delimiters" % path)
    starts.append(len(data))
    with open(out, "wb") as f:
        for a, b in zip(starts, starts[1:]):
            f.write(struct.pack("<I", b - a))
            f.write(data[a:b])
    print("%s: %d units, %d bytes" % (out, len(starts) - 1, len(data)))


def cut(dump, index, count, out):
    data = open(dump, "rb").read()
    lengths = [int(line) for line in open(index) if line.strip()][:count]
    at = 0
    with open(out, "wb") as f:
        for length in lengths:
            f.write(struct.pack("<I", length))
            f.write(data[at : at + length])
            at += length
    print("%s: %d units, %d bytes" % (out, len(lengths), at + 4 * len(lengths)))


def sums(clip, codec, pix_fmt, out):
    stream = b"".join(units(clip))
    probe = subprocess.run(
        ["ffprobe", "-v", "error", "-f", codec, "-show_entries",
         "stream=width,height", "-of", "csv=p=0", "-"],
        input=stream, capture_output=True, check=True,
    )
    width, height = (int(v) for v in probe.stdout.decode().strip().split(","))
    raw = subprocess.run(
        ["ffmpeg", "-v", "error", "-f", codec, "-i", "-", "-fps_mode", "passthrough",
         "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"],
        input=stream, capture_output=True, check=True,
    ).stdout
    sample = 2 if pix_fmt == "p010le" else 1
    y_bytes = width * height * sample
    uv_bytes = width * (height // 2) * sample
    frame_bytes = y_bytes + uv_bytes
    if len(raw) % frame_bytes:
        sys.exit("raw output is not a whole number of %dx%d pictures" % (width, height))
    with open(out, "w") as f:
        for n in range(len(raw) // frame_bytes):
            frame = raw[n * frame_bytes : (n + 1) * frame_bytes]
            f.write("%d %d %d\n" % (n, zlib.crc32(frame[:y_bytes]), zlib.crc32(frame[y_bytes:])))
    print("%s: %d pictures at %dx%d" % (out, len(raw) // frame_bytes, width, height))


if __name__ == "__main__":
    if len(sys.argv) == 6 and sys.argv[1] == "cut":
        cut(sys.argv[2], sys.argv[3], int(sys.argv[4]), sys.argv[5])
    elif len(sys.argv) == 5 and sys.argv[1] == "annexb":
        annexb(sys.argv[2], sys.argv[3], sys.argv[4])
    elif len(sys.argv) == 6 and sys.argv[1] == "sums":
        sums(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5])
    else:
        sys.exit(__doc__)
