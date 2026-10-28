#!/bin/bash
# Assemble the Wine install tree the wasm runtime runs from ($VNVM_WINE_ROOT, default
# /opt/wine-vnbox): vnbox's i386 PE files, plus the i386 builds of the programs vnbox (a wow64
# build) only ships for x86_64 (wineboot, services, ...), built from the same source.
#
# env: VNBOX_WINE (vnbox's installed Wine), WINE_SRC (patched Wine source), WINE_TOOLS,
#      PE_BUILD (build dir for the i386 programs), ROOT_OUT (the tree to create)
set -e
VNBOX_WINE=${VNBOX_WINE:-/home/user/img/rootfs/opt/wine-vnbox}
WINE_SRC=${WINE_SRC:-/home/user/work/wine-src}
WINE_TOOLS=${WINE_TOOLS:-/home/user/img/buildroot/tmp/vnbox-build/wine-build-src/build}
PE_BUILD=${PE_BUILD:-/home/user/work/wine-pe32}
ROOT_OUT=${ROOT_OUT:-/home/user/work/wineroot}
EMSDK4=${EMSDK4:-/home/user/emsdk4}
. "$EMSDK4/emsdk_env.sh" >/dev/null 2>&1

# x86_64-only in vnbox; needs i686-w64-mingw32-gcc
PROGRAMS="conhost lsass plugplay rpcss services spoolsv termsv wineboot winemenubuilder wuauserv"
# ours (patches/wine-wasm.patch)
DLLS="winebrowser.drv"

mkdir -p "$PE_BUILD"
cd "$PE_BUILD"
if [ ! -f Makefile ] || [ "$WINE_SRC/configure" -nt Makefile ]; then
  emconfigure "$WINE_SRC/configure" --host=i386-pc-linux-gnu --build=x86_64-pc-linux-gnu \
    --with-wine-tools="$WINE_TOOLS" --prefix=/opt/wine-vnbox --disable-tests --without-x \
    --without-freetype --without-wayland --without-vulkan --without-opengl --without-pulse \
    --without-alsa --without-oss --without-gstreamer --without-ffmpeg --without-cups --without-dbus \
    --without-gnutls --without-krb5 --without-netapi --without-sdl --without-udev --without-usb \
    --without-v4l2 --without-capi --without-pcap --without-pcsclite --without-sane --without-gphoto \
    --without-inotify --without-fontconfig --without-xinput2 CFLAGS="-O2 -D__i386__ -pthread -fPIC" \
    > configure.log 2>&1
fi
make -j"$(nproc)" $(for p in $PROGRAMS; do echo programs/$p/i386-windows/$p.exe; done) \
  $(for d in $DLLS; do echo dlls/$d/i386-windows/$d; done) > make.log 2>&1

PE=$ROOT_OUT/lib/wine/i386-windows
rm -rf "$PE" && mkdir -p "$PE" "$ROOT_OUT/lib/wine/i386-unix" "$ROOT_OUT/share"
for f in "$VNBOX_WINE"/lib/wine/i386-windows/*; do ln -s "$f" "$PE/"; done
for p in $PROGRAMS; do
  i686-w64-mingw32-strip -o "$PE/$p.exe" "$PE_BUILD/programs/$p/i386-windows/$p.exe"
done
for d in $DLLS; do
  i686-w64-mingw32-strip -o "$PE/$d" "$PE_BUILD/dlls/$d/i386-windows/$d"
done
# ntdll looks for its own .so to find the install; the code itself is in wineunix.so
: > "$ROOT_OUT/lib/wine/i386-unix/ntdll.so"
ln -sfn "$VNBOX_WINE/share/wine" "$ROOT_OUT/share/wine"
echo "wine root in $ROOT_OUT"
