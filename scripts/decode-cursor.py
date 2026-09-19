#!/usr/bin/env python3
"""Make the reference pixels for the committed pointer pictures.

A pointer picture travels as a PNG and the client's reader turns it into
RGBA. Beside each committed picture sits its pixels as an independent decoder
produced them, run as a separate process, so the reader under test cannot
bless its own output.

    scripts/decode-cursor.py <picture.png> [<picture.png> ...]

Writes `<picture>.rgba` next to each input: width * height * 4 bytes of
red, green, blue, alpha, rows top to bottom, straight alpha.
"""

import subprocess
import sys


def main(paths):
    for path in paths:
        out = path[: -len(".png")] + ".rgba" if path.endswith(".png") else path + ".rgba"
        subprocess.run(
            [
                "ffmpeg",
                "-v",
                "error",
                "-y",
                "-i",
                path,
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                out,
            ],
            check=True,
        )
        print(f"{path} -> {out}")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    main(sys.argv[1:])
