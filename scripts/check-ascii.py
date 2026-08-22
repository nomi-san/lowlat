#!/usr/bin/env python3
"""Enforce the ASCII-only rule (AGENTS.md 1).

Source, docs, config, and scripts must be plain ASCII. The section sign is the
single permitted exception, and only for section references.

Usage:
    python scripts/check-ascii.py            # check tracked files
    python scripts/check-ascii.py path ...   # check specific paths

Exit code is 1 if any violation is found.
"""

import os
import subprocess
import sys

ALLOWED = {"§"}

EXTENSIONS = {
    ".rs", ".md", ".toml", ".py", ".sh", ".c", ".h", ".ts", ".js",
    ".json", ".yml", ".yaml", ".glsl", ".hlsl", ".comp", ".service",
    ".cs", ".csproj",
}

BASENAMES = {
    "Makefile",
    "Dockerfile",
    ".gitignore",
    ".gitattributes",
    # Git hooks are extensionless but are source we own.
    "pre-commit",
    "pre-push",
    "commit-msg",
}

SKIP_DIRS = {".git", "target", "node_modules", "third_party", "local", ".claude"}

NAMES = {
    0x2013: "en dash",
    0x2014: "em dash",
    0x2018: "left single quote",
    0x2019: "right single quote",
    0x201c: "left double quote",
    0x201d: "right double quote",
    0x2022: "bullet",
    0x2026: "ellipsis",
    0x00b7: "middle dot",
    0x00b0: "degree sign",
    0x00b5: "micro sign",
    0x00d7: "multiplication sign",
    0x00b1: "plus-minus",
    0x2192: "rightwards arrow",
    0x2190: "leftwards arrow",
    0x2265: "greater-than or equal",
    0x2264: "less-than or equal",
    0x2713: "check mark",
    0x00a0: "non-breaking space",
}

SUGGEST = {
    0x2013: "-",
    0x2014: "--",
    0x2018: "'",
    0x2019: "'",
    0x201c: '"',
    0x201d: '"',
    0x2022: "-",
    0x2026: "...",
    0x00b7: "*",
    0x00b0: "deg",
    0x00b5: "u",
    0x00d7: "x",
    0x00b1: "+/-",
    0x2192: "->",
    0x2190: "<-",
    0x2265: ">=",
    0x2264: "<=",
    0x2713: "ok",
    0x00a0: "space",
}


def wanted(path):
    base = os.path.basename(path)
    if base in BASENAMES:
        return True
    return os.path.splitext(path)[1] in EXTENSIONS


def skipped(path):
    """True for a path inside a directory the rule does not cover."""
    return any(part in SKIP_DIRS for part in path.replace(os.sep, "/").split("/"))


def tracked_files():
    try:
        out = subprocess.run(
            ["git", "ls-files"], capture_output=True, text=True, check=True
        ).stdout
    except (subprocess.CalledProcessError, FileNotFoundError):
        return walk_files(".")
    # SKIP_DIRS has to be applied here as well as in the walk. It was not, and
    # nothing noticed while the only vendored code was a submodule, which lists
    # as a single entry rather than as its contents. The first vendored file
    # tracked directly turned an upstream header's typography into a failure of
    # our own rule, which covers what we write and not what we carry.
    return [p for p in out.splitlines() if p and not skipped(p)]


def walk_files(root):
    found = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            found.append(os.path.join(dirpath, name))
    return found


def check(path):
    violations = []
    try:
        with open(path, "r", encoding="utf-8") as handle:
            lines = handle.readlines()
    except (UnicodeDecodeError, OSError):
        return [(0, 0, -1, "not valid UTF-8 text")]
    for lineno, line in enumerate(lines, 1):
        for col, ch in enumerate(line, 1):
            point = ord(ch)
            if point > 127 and ch not in ALLOWED:
                name = NAMES.get(point, "non-ASCII")
                fix = SUGGEST.get(point)
                detail = name if fix is None else "%s, write %s" % (name, fix)
                violations.append((lineno, col, point, detail))
    return violations


def expand(paths):
    """Directories expand to their contents; files pass through."""
    out = []
    for path in paths:
        if os.path.isdir(path):
            out.extend(walk_files(path))
        else:
            out.append(path)
    return out


def main(argv):
    if len(argv) > 1:
        paths = expand(argv[1:])
        if not paths:
            print("no such path: %s" % " ".join(argv[1:]))
            return 1
    else:
        paths = tracked_files()
    total = 0
    checked = 0
    for path in paths:
        if not os.path.isfile(path) or not wanted(path):
            continue
        checked += 1
        for lineno, col, point, detail in check(path):
            if point < 0:
                print("%s: %s" % (path, detail))
            else:
                print("%s:%d:%d: U+%04X %s" % (path, lineno, col, point, detail))
            total += 1
    if total:
        print("\n%d violation(s) in %d file(s) checked" % (total, checked))
        print("See AGENTS.md 1. The section sign is the only permitted exception.")
        return 1
    print("ascii check clean (%d files)" % checked)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
