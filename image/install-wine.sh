#!/bin/bash
# Put vnbox's Wine (image/build-wine.sh) into the guest rootfs in place of
# Ubuntu's, and create the prefix the way vnbox's mkbase does
# (internal/base/base.go): win10, code page 932, its registry settings, font
# replacements and the renamed IPA fonts (msfonts). vnbox's icu verb is left
# out: it downloads ICU from GitHub releases, which the build host may not
# reach.
#
#   image/install-wine.sh ROOTFS WINE_DIR     (WINE_DIR = BUILDROOT/opt/wine-vnbox)
set -euo pipefail
R=$(realpath "${1:?}")
W=$(realpath "${2:?}")
HERE=$(cd "$(dirname "$0")" && pwd)

cleanup() { umount "$R/proc" "$R/sys" "$R/dev" 2>/dev/null || true; }
trap cleanup EXIT
mount -t proc proc "$R/proc"; mount --bind /sys "$R/sys"; mount --bind /dev "$R/dev"
cp /etc/resolv.conf "$R/etc/resolv.conf" 2>/dev/null || true

PKGS=$(grep -v '^#' "$HERE/wine-runtime-packages.txt" | tr '\n' ' ')
chroot "$R" /bin/bash -euo pipefail <<EOF2
export DEBIAN_FRONTEND=noninteractive
apt-get purge -y -qq 'wine*' 'libwine*' fonts-wine >/dev/null 2>&1 || true
apt-get update -qq
apt-get install -y -qq --no-install-recommends $PKGS
# New WoW64: 32-bit games run without any i386 libraries.
apt-get purge -y -qq '.*:i386' >/dev/null 2>&1 || true
dpkg --remove-architecture i386 2>/dev/null || true
apt-get autoremove -y -qq --purge >/dev/null
apt-get clean
rm -rf /var/lib/apt/lists/*
EOF2

rm -rf "$R/opt/wine-vnbox"
cp -a "$W" "$R/opt/wine-vnbox"
ln -sf /opt/wine-vnbox/bin/{wine,wineserver,wineboot,winecfg,regedit} "$R/usr/local/bin/"

rm -rf "$R/root/.wine"
chroot "$R" /bin/bash -euo pipefail <<'EOF2'
export WINEPREFIX=/root/.wine WINEDEBUG=-all WINEDLLOVERRIDES="mscoree,mshtml=;winemenubuilder.exe=d"
export LANG=ja_JP.UTF-8 LC_ALL=ja_JP.UTF-8 TZ=Asia/Tokyo
unset DISPLAY
wineboot -i
wineserver -w
r() { wine reg add "$@" /f >/dev/null; }
r 'HKCU\Software\Wine' /v Version /d win10
r 'HKCU\Software\Wine\X11 Driver' /v UseXRandR /d N
r 'HKCU\Software\Wine\X11 Driver' /v UseXVidMode /d N
r 'HKLM\Software\Microsoft\Windows NT\CurrentVersion\AeDebug' /v Auto /d 0
r 'HKLM\Software\Microsoft\Windows NT\CurrentVersion\AeDebug' /v Debugger /d ''
r 'HKCU\Software\Wine\WineDbg' /v ShowCrashDialog /t REG_DWORD /d 0
r 'HKCU\Software\Wine\DllOverrides' /v vulkan-1 /d native,builtin
F='HKCU\Software\Wine\Fonts\Replacements'
r "$F" /v 'MS Gothic' /d IPAGothic;  r "$F" /v 'MS PGothic' /d IPAPGothic; r "$F" /v 'MS UI Gothic' /d IPAPGothic
r "$F" /v 'MS Mincho' /d IPAMincho;  r "$F" /v 'MS PMincho' /d IPAPMincho
r "$F" /v 'ＭＳ ゴシック' /d IPAGothic; r "$F" /v 'ＭＳ Ｐゴシック' /d IPAPGothic
r "$F" /v 'ＭＳ 明朝' /d IPAMincho;   r "$F" /v 'ＭＳ Ｐ明朝' /d IPAPMincho
for n in Meiryo 'Meiryo UI' 'メイリオ' 'Yu Gothic' 'Yu Gothic UI' '游ゴシック'; do r "$F" /v "$n" /d 'Noto Sans CJK JP'; done
for n in 'Yu Mincho' '游明朝'; do r "$F" /v "$n" /d 'Noto Serif CJK JP'; done
wineserver -w
EOF2
python3 "$HERE/msfonts.py" "$R" /root/.wine > "$R/tmp/msfonts.reg"
chroot "$R" /bin/bash -euo pipefail <<'EOF2'
export WINEPREFIX=/root/.wine WINEDEBUG=-all LANG=ja_JP.UTF-8 LC_ALL=ja_JP.UTF-8
wine regedit /S 'Z:\tmp\msfonts.reg'
wineserver -w
rm /tmp/msfonts.reg
wine reg query 'HKLM\System\CurrentControlSet\Control\Nls\CodePage' /v ACP | grep -q 932 ||
  { echo "code page is not 932"; exit 1; }
EOF2
python3 "$HERE/dedupe-prefix.py" "$R"
echo "vnbox Wine installed: $(chroot "$R" /opt/wine-vnbox/bin/wine --version)"
