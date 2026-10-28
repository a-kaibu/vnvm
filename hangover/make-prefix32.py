#!/usr/bin/env python3
"""Derive a 32-bit (win32) Wine prefix from a WoW64 prefix made by vnbox's Wine.

The wasm build of Wine is a plain i386 Wine, which refuses win64 prefixes. File data is
hard-linked; system32 becomes the old syswow64, and the registry's Wow6432Node keys move to
their native location (overriding the 64-bit values).

usage: make-prefix32.py SRC_PREFIX DST_PREFIX
"""
import os, re, shutil, subprocess, sys

src, dst = sys.argv[1], sys.argv[2]
if os.path.exists(dst):
    sys.exit(f"{dst} exists")
subprocess.run(["cp", "-al", src, dst], check=True)
win = os.path.join(dst, "drive_c", "windows")
shutil.rmtree(os.path.join(win, "system32"))
os.rename(os.path.join(win, "syswow64"), os.path.join(win, "system32"))
for name in ("system.reg", "user.reg", "userdef.reg"):
    path = os.path.join(dst, name)
    text = open(path, encoding="utf-8", errors="surrogateescape").read()
    os.unlink(path)  # break the hard link before writing
    text = text.replace("#arch=win64", "#arch=win32", 1)
    head, *sections = re.split(r"(?m)^(?=\[)", text)
    native, wow = [], []
    for s in sections:
        key = s[1:s.index("]")]
        if "\\\\Wow6432Node" in key:
            if "\n#link" not in s:  # 32-bit views that link back to the 64-bit keys
                wow.append(s.replace("\\\\Wow6432Node", "", 1))
        else:
            native.append(s)
    out = head + "".join(native) + "".join(wow)
    out = out.replace("C:\\\\Program Files (x86)", "C:\\\\Program Files")
    open(path, "w", encoding="utf-8", errors="surrogateescape").write(out)
print("ok")
