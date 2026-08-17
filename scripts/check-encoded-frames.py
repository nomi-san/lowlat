#!/usr/bin/env python3
"""Check an encoded stream against the synthetic source that produced it.

The synthetic source draws a bright vertical bar whose left edge is
`(index * step) % width`. That is the whole content contract, so a decoded
picture can be checked against nothing but its frame number: this script and
the encoder share no state, and an independent decoder sits between them.

What it catches that a structural check cannot: a wrong upload stride shears
the picture, a swapped plane offset moves the bar vertically or destroys it,
and an off-by-one in the frame order shows as a bar that is one step out.
All three produce a stream that parses, decodes, and reports the right
resolution.

Usage:
    scripts/check-encoded-frames.py /tmp/vaapi.h264
    FFMPEG=/path/to/ffmpeg scripts/check-encoded-frames.py dump.h264 --width 1920

The bar geometry mirrors constants in crates/capture/src/synthetic.rs. If they
are changed there, the defaults here follow.
"""

import argparse
import os
import subprocess
import sys
import tempfile

BAR_WIDTH = 64
BAR_STEP = 17
# Halfway between the background and the bar, so a decoded sample lands on the
# correct side of it despite quantisation.
THRESHOLD = 150


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("stream", help="an elementary stream written by an encode test")
    parser.add_argument("--width", type=int, default=1920)
    parser.add_argument("--height", type=int, default=1080)
    parser.add_argument("--step", type=int, default=BAR_STEP)
    parser.add_argument("--bar", type=int, default=BAR_WIDTH)
    parser.add_argument(
        "--row",
        type=int,
        default=None,
        help="which luma row to sample; defaults to the middle of the picture",
    )
    args = parser.parse_args()

    ffmpeg = os.environ.get("FFMPEG", "ffmpeg")
    row = args.row if args.row is not None else args.height // 2

    with tempfile.NamedTemporaryFile(suffix=".gray") as raw:
        try:
            subprocess.run(
                [ffmpeg, "-v", "error", "-y", "-i", args.stream,
                 "-pix_fmt", "gray", "-f", "rawvideo", raw.name],
                check=True,
            )
        except FileNotFoundError:
            print(f"no decoder: {ffmpeg} not found; set FFMPEG to one", file=sys.stderr)
            return 2
        except subprocess.CalledProcessError:
            print("the decoder refused the stream", file=sys.stderr)
            return 1
        pixels = open(raw.name, "rb").read()

    frame_bytes = args.width * args.height
    if frame_bytes == 0 or len(pixels) < frame_bytes:
        print("the decoder produced no complete frame", file=sys.stderr)
        return 1

    frames = len(pixels) // frame_bytes
    failures = 0
    for index in range(frames):
        start = index * frame_bytes + row * args.width
        scanline = pixels[start:start + args.width]
        lit = [x for x, value in enumerate(scanline) if value > THRESHOLD]
        expected = (index * args.step) % args.width
        # The bar wraps, and a wrapped bar is two runs rather than one; only
        # the unwrapped case has a single contiguous span to compare against.
        wraps = expected + args.bar > args.width
        if wraps:
            print(f"frame {index}: bar wraps, skipped")
            continue
        if not lit or min(lit) != expected or max(lit) != expected + args.bar - 1:
            span = f"{min(lit)}..{max(lit)}" if lit else "none"
            print(
                f"frame {index}: FAIL bar at {span}, "
                f"expected {expected}..{expected + args.bar - 1}"
            )
            failures += 1
        else:
            print(f"frame {index}: ok bar at {expected}..{expected + args.bar - 1}")

    print(f"{frames - failures}/{frames} frames match the source")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
