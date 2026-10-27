#!/usr/bin/env python3
"""Hardlink Wine prefix DLLs to the identical files Wine ships in /usr/lib.

Wine copies every builtin PE module into the prefix, doubling ~1GB of disk and
page cache. Hardlinks keep one copy; Wine does not care.

    dedupe-prefix.py ROOTFS
"""
import hashlib, os, sys

root = sys.argv[1]
src_dirs = ["opt/wine-vnbox/lib/wine"]
prefix = os.path.join(root, "root/.wine/drive_c/windows")

def digest(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()

by_key = {}
for d in src_dirs:
    for dp, _, fs in os.walk(os.path.join(root, d)):
        for f in fs:
            p = os.path.join(dp, f)
            if os.path.isfile(p) and not os.path.islink(p):
                by_key.setdefault((f.lower(), os.path.getsize(p)), []).append(p)

saved = linked = 0
for dp, _, fs in os.walk(prefix):
    for f in fs:
        p = os.path.join(dp, f)
        if os.path.islink(p) or not os.path.isfile(p):
            continue
        cands = by_key.get((f.lower(), os.path.getsize(p)))
        if not cands:
            continue
        d = digest(p)
        for c in cands:
            if os.stat(c).st_ino != os.stat(p).st_ino and digest(c) == d:
                tmp = p + ".vnvm-tmp"
                os.link(c, tmp)
                os.replace(tmp, p)
                saved += os.path.getsize(c)
                linked += 1
                break
print(f"hardlinked {linked} files, saved {saved >> 20} MiB")
