#!/usr/bin/env python3
"""vnbox's msfonts verb (internal/verbs/fonts.go): copies of the IPA fonts
renamed to the Windows Japanese font names, registered in the prefix. Wine's
font replacements do not show up in EnumFontFamiliesEx, so games that look a
font up by name need real ones.

  msfonts.py ROOTFS PREFIX_IN_ROOTFS   -> writes the .ttf files and prints a .reg
"""
import sys
from fontTools.ttLib import TTFont
from fontTools.ttLib.tables._n_a_m_e import NameRecord

FONTS = [  # file, source, English name, Japanese name
    ("msgothic.ttf", "/usr/share/fonts/opentype/ipafont-gothic/ipag.ttf", "MS Gothic", "ＭＳ ゴシック"),
    ("mspgothic.ttf", "/usr/share/fonts/opentype/ipafont-gothic/ipagp.ttf", "MS PGothic", "ＭＳ Ｐゴシック"),
    ("msuigothic.ttf", "/usr/share/fonts/opentype/ipafont-gothic/ipagp.ttf", "MS UI Gothic", "MS UI Gothic"),
    ("msmincho.ttf", "/usr/share/fonts/opentype/ipafont-mincho/ipam.ttf", "MS Mincho", "ＭＳ 明朝"),
    ("mspmincho.ttf", "/usr/share/fonts/opentype/ipafont-mincho/ipamp.ttf", "MS PMincho", "ＭＳ Ｐ明朝"),
]


def rename(src, dst, en, ja):
    f = TTFont(src)
    name = f["name"]
    # Drop every family / unique / full / PostScript / typographic name, then set ours.
    name.names = [r for r in name.names if r.nameID not in (1, 3, 4, 6, 16, 17, 18)]

    def put(nid, lang, s):
        name.names = [r for r in name.names if not (r.platformID == 3 and r.platEncID == 1 and r.langID == lang and r.nameID == nid)]
        r = NameRecord()
        r.nameID, r.platformID, r.platEncID, r.langID = nid, 3, 1, lang
        r.string = s
        name.names.append(r)

    for lang, label in ((0x409, en), (0x411, ja)):
        put(1, lang, label)
        put(4, lang, label)
        put(2, lang, "Regular")
        put(3, lang, label + ";vnbox")
    put(6, 0x409, en.replace(" ", ""))
    f.save(dst)


root, prefix = sys.argv[1], sys.argv[2]
print("Windows Registry Editor Version 5.00\n")
print(r"[HKEY_LOCAL_MACHINE\Software\Microsoft\Windows NT\CurrentVersion\Fonts]")
for file, src, en, ja in FONTS:
    rename(root + src, f"{root}{prefix}/drive_c/windows/Fonts/{file}", en, ja)
    print(f'"{en} (TrueType)"="{file}"')
# The real font wins over a replacement of the same name.
print("\n[HKEY_CURRENT_USER\\Software\\Wine\\Fonts\\Replacements]")
for _, _, en, _ in FONTS:
    print(f'"{en}"=-')
