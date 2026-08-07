#!/usr/bin/env python3
"""Byte-exact literal check for binaries. Replaces grep in verified-deploy.sh.

Why not grep: BSD grep (macOS /usr/bin/grep 2.6.0-FreeBSD) does NOT find UTF-8
CJK literals in binary data -- verified 2026-08-07 across default BRE / -F /
-f pattern-file, direct argv and via bash+zsh: all 9 combinations NOT FOUND,
while the literal is provably present (55 bytes, offset 14344995, count=1).
GNU grep 3.12 finds it. So the result depended on which grep was first in PATH
=> the deploy gate false-negatived a correct binary.

Pattern arrives base64-encoded so no shell/ssh quoting layer can corrupt the
multibyte bytes.

usage: has_literal.py <file> <base64-of-utf8-literal>
exit 0 = present, 1 = absent, 2 = usage/IO error
"""
import base64
import sys


def main() -> int:
    if len(sys.argv) != 3:
        sys.stderr.write("usage: has_literal.py <file> <base64-literal>\n")
        return 2
    path, b64 = sys.argv[1], sys.argv[2]
    try:
        needle = base64.b64decode(b64)
        with open(path, "rb") as fh:
            blob = fh.read()
    except Exception as e:  # noqa: BLE001 - deploy gate: surface anything
        sys.stderr.write("has_literal.py error: %s\n" % e)
        return 2
    n = blob.count(needle)
    sys.stderr.write("    [byte-scan] count=%d needle=%dB file=%s\n"
                     % (n, len(needle), path))
    return 0 if n else 1


if __name__ == "__main__":
    sys.exit(main())
