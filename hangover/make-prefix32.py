#!/usr/bin/env python3
"""Derive a 32-bit (win32) Wine prefix from a WoW64 prefix made by vnbox's Wine.

The wasm build of Wine is a plain i386 Wine, which refuses win64 prefixes. File data is
hard-linked; system32 becomes the old syswow64, and the registry's Wow6432Node keys move to
their native location (overriding the 64-bit values). system32\\drivers (64-bit only in a WoW64
prefix) gets the i386 drivers of the Wine root.

usage: make-prefix32.py SRC_PREFIX DST_PREFIX [WINE_ROOT (default /opt/wine-vnbox)]
"""
import os, re, shutil, subprocess, sys

# Every page load boots Wine afresh, and each process costs seconds there: services a VN does
# not use stay stopped (event log in svchost, the network proxy driver), and the USB, Bluetooth
# and HID bus drivers (a winedevice of their own, with no backend in the browser) are not
# installed. The mount manager too: its winedevice held wineboot (and so the game) up for about
# a second, and without it drives are simply fixed disks with no volume serial.
MANUAL = ("Eventlog", "NDIS", "nsiproxy", "MountMgr")
DROP = re.compile(r"\[System\\\\ControlSet001\\\\Enum\\\\ROOT\\\\WINE\\\\(winebus|wineusb|winebth)[]\\\\]")

src, dst = sys.argv[1], sys.argv[2]
wine_root = sys.argv[3] if len(sys.argv) > 3 else "/opt/wine-vnbox"
if os.path.exists(dst):
    sys.exit(f"{dst} exists")
subprocess.run(["cp", "-al", src, dst], check=True)
win = os.path.join(dst, "drive_c", "windows")
drivers64 = os.listdir(os.path.join(win, "system32", "drivers"))
shutil.rmtree(os.path.join(win, "system32"))
os.rename(os.path.join(win, "syswow64"), os.path.join(win, "system32"))
drivers = os.path.join(win, "system32", "drivers")
os.makedirs(drivers, exist_ok=True)
for name in drivers64:
    lib = os.path.join(wine_root, "lib", "wine", "i386-windows", name)
    if os.path.isfile(lib) and not os.path.exists(os.path.join(drivers, name)):
        shutil.copy(lib, os.path.join(drivers, name))
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
        elif not DROP.match(s):
            native.append(s)
    out = head + "".join(native) + "".join(wow)
    out = out.replace("C:\\\\Program Files (x86)", "C:\\\\Program Files")
    for svc in MANUAL:
        out = re.sub(r'(\[System\\\\ControlSet001\\\\Services\\\\' + svc + r'\][^[]*?"Start"=dword:)0000000[0-2]',
                     r"\g<1>00000003", out)
    # the desktop (explorer) starts with wineboot instead of when the game makes its first
    # window, so its startup overlaps the boot (winemenubuilder isn't there anyway)
    out = re.sub(r'(\[Software\\\\Microsoft\\\\Windows\\\\CurrentVersion\\\\RunServices\][^[]*?)"winemenubuilder"="[^"\n]*"',
                 r'\g<1>"desktop"="C:\\\\windows\\\\system32\\\\explorer.exe /desktop"', out)
    if name == "user.reg":
        # the browser display driver (dlls/winebrowser.drv in the Wine patch)
        out += '\n[Software\\\\Wine\\\\Drivers]\n"Audio"="browser"\n"Graphics"="browser"\n'
    open(path, "w", encoding="utf-8", errors="surrogateescape").write(out)
print("ok")
