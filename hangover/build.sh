#!/bin/bash
# Build the wasm Wine runtime (Hangover-style): Wine's unix side + wineserver + vnhost.
#
# env: EMSDK4 (emsdk with emscripten >= 4.0), WINE_SRC (patched Wine source),
#      WINE_TOOLS (native Wine build tree for winebuild/makedep), BUILD (build dir), OUT
set -e
EMSDK4=${EMSDK4:-/home/user/emsdk4}
WINE_SRC=${WINE_SRC:-/home/user/work/wine-src}
WINE_TOOLS=${WINE_TOOLS:-/home/user/img/buildroot/tmp/vnbox-build/wine-build-src/build}
BUILD=${BUILD:-/home/user/work/wine-wb4}
OUT=${OUT:-/home/user/work/hangover-out}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(dirname "$HERE")
. "$EMSDK4/emsdk_env.sh" >/dev/null 2>&1

mkdir -p "$BUILD" "$OUT"
cd "$BUILD"
if [ ! -f Makefile ]; then
  emconfigure "$WINE_SRC/configure" --host=i386-pc-linux-gnu --build=x86_64-pc-linux-gnu \
    --with-wine-tools="$WINE_TOOLS" --prefix=/opt/wine-vnbox --disable-tests --without-mingw \
    --without-x --without-freetype --without-wayland --without-vulkan --without-opengl \
    --without-pulse --without-alsa --without-oss --without-gstreamer --without-ffmpeg \
    --without-cups --without-dbus --without-gnutls --without-krb5 --without-netapi --without-sdl \
    --without-udev --without-usb --without-v4l2 --without-capi --without-pcap --without-pcsclite \
    --without-sane --without-gphoto --without-inotify --without-fontconfig --without-xinput2 \
    CFLAGS="-O2 -D__i386__ -pthread" > configure.log 2>&1
fi
make -j"$(nproc)" dlls/ntdll/ntdll.so > make.log 2>&1
# the server objects; linking wineserver on its own fails (it lives in our module) and that's fine
make -j"$(nproc)" -k server/wineserver >> make.log 2>&1 || true

cd "$ROOT"
RUSTC_BOOTSTRAP=1 RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals" \
  cargo build --release -p vnhost --target wasm32-unknown-emscripten -Zbuild-std=std,panic_abort
VNHOST=$ROOT/target/wasm32-unknown-emscripten/release/libvnhost.a

WRAP="socket socketpair pipe pipe2 bind listen accept accept4 connect shutdown setsockopt getsockopt
  getsockname getpeername sendmsg recvmsg send recv sendto recvfrom read write readv writev close dup dup2
  poll fstat lseek fcntl ioctl mmap munmap mprotect madvise msync mlock mremap dlopen dlsym dlclose
  dlerror dladdr kill pthread_kill isatty"
WRAPFLAGS=$(for w in $WRAP; do printf -- "-Wl,--wrap=%s " "$w"; done)

emcc -O2 -pthread -c "$HERE/main.c" -o "$OUT/main.o"
emcc -O2 -pthread -c "$ROOT/crates/vnhost/c/shim.c" -o "$OUT/shim.o"
emcc -O2 -pthread -o "$OUT/wine.js" "$OUT/main.o" "$OUT/shim.o" \
  "$BUILD/dlls/ntdll/ntdll.so" $(cd "$BUILD" && ls server/*.o | sed "s|^|$BUILD/|") "$VNHOST" \
  $WRAPFLAGS --pre-js "$HERE/pre.js" \
  -sPROXY_TO_PTHREAD -sEXIT_RUNTIME -sNODERAWFS=1 -sENVIRONMENT=node \
  -sGLOBAL_BASE=2147483648 -sINITIAL_MEMORY=2214592512 -sMAXIMUM_MEMORY=4GB -sALLOW_MEMORY_GROWTH=1 \
  -sSTACK_SIZE=1MB -sDEFAULT_PTHREAD_STACK_SIZE=1MB -sPTHREAD_POOL_SIZE=8 -sPTHREAD_POOL_SIZE_STRICT=0 \
  -sERROR_ON_UNDEFINED_SYMBOLS=1 2> "$OUT/link.log" || { grep -i "undefined\|error" "$OUT/link.log" | sort -u | head -80; exit 1; }
echo "built $OUT/wine.js"
