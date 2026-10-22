#!/usr/bin/env python3
"""Fetch nefcon, the device tool the drivers' install scripts run.

nefcon (github.com/nefarius/nefcon, MIT) makes the drivers' root devices,
installs and updates their packages and removes both; Windows' own tool cannot
make a root device. It is pinned by version and by the SHA-256 digest its
release publishes, and unpacked into target/nefcon/<version>, so a staged
folder carries it with its licence. A second run with the release already
unpacked does nothing.

Usage:
    python scripts/fetch-nefcon.py         # print the console tool's path
"""

import hashlib
import os
import shutil
import sys
import tempfile
import urllib.request
import zipfile

VERSION = "1.21.0"
# The release's own figures for its one asset.
SHA256 = "4c8e68f6b663598d77b7c3f76459b16fad00994e53ea8af0bd106a805dd5c18b"
SIZE = 2739422

URL = "https://github.com/nefarius/nefcon/releases/download/v{0}/nefcon_v{0}.zip".format(VERSION)
# The licence, which the release does not carry, at the release's tag: a
# folder that ships the tool ships it beside it.
LICENSE_URL = "https://raw.githubusercontent.com/nefarius/nefcon/v{0}/LICENSE".format(VERSION)
LICENSE_SHA256 = "6e07dc26b416c292d55516d33f0e6fbd4567c250c65d11095b85199d98fb8e23"
MARKER = ".lowlat-unpacked"
# The console build, for scripts whose output a person reads.
TOOL = os.path.join("x64", "nefconc.exe")


def root():
    return os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), ".."))


def download(url, path, sha256, size=None):
    request = urllib.request.Request(url, headers={"User-Agent": "lowlat-fetch-nefcon"})
    digest = hashlib.sha256()
    got = 0
    with urllib.request.urlopen(request, timeout=120) as response, open(path, "wb") as out:
        while True:
            block = response.read(1 << 20)
            if not block:
                break
            digest.update(block)
            got += len(block)
            out.write(block)
    if size is not None and got != size:
        raise SystemExit("fetch-nefcon: %s is %d bytes, expected %d" % (url, got, size))
    if digest.hexdigest() != sha256:
        raise SystemExit("fetch-nefcon: digest mismatch for %s: %s" % (url, digest.hexdigest()))


def unpack(archive, dest):
    base = os.path.realpath(dest)
    with zipfile.ZipFile(archive) as z:
        for info in z.infolist():
            # Nothing may land outside the destination.
            target = os.path.realpath(os.path.join(base, info.filename))
            if os.path.commonpath([base, target]) != base:
                raise SystemExit("fetch-nefcon: entry escapes the destination: %r" % info.filename)
            if info.filename.endswith("/"):
                os.makedirs(target, exist_ok=True)
                continue
            os.makedirs(os.path.dirname(target), exist_ok=True)
            with z.open(info) as src, open(target, "wb") as out:
                shutil.copyfileobj(src, out)


def main():
    dest = os.path.join(root(), "target", "nefcon", VERSION)
    marker = os.path.join(dest, MARKER)
    tool = os.path.join(dest, TOOL)
    if os.path.exists(marker) and open(marker, encoding="ascii").read().strip() == SHA256:
        print(tool)
        return
    if os.path.exists(dest):
        shutil.rmtree(dest)
    os.makedirs(dest)
    fd, archive = tempfile.mkstemp(suffix=".zip", dir=os.path.dirname(dest))
    os.close(fd)
    try:
        print("fetch-nefcon: downloading nefcon %s" % VERSION, file=sys.stderr)
        download(URL, archive, SHA256, SIZE)
        unpack(archive, dest)
    finally:
        os.remove(archive)
    download(LICENSE_URL, os.path.join(dest, "LICENSE"), LICENSE_SHA256)
    if not os.path.isfile(tool):
        raise SystemExit("fetch-nefcon: the release has no %s" % TOOL)
    with open(marker, "w", encoding="ascii") as out:
        out.write(SHA256 + "\n")
    print(tool)


if __name__ == "__main__":
    main()
