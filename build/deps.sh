#!/bin/bash
# Build the emscripten-compiled libraries QEMU-wasm links against (zlib, libffi,
# glib, pixman). Mirrors ktock/qemu-wasm's Dockerfile without needing Docker.
#
#   EMSDK=/path/to/emsdk WORK=/path/to/work build/deps.sh
#
# Output: $WORK/target/{include,lib}
set -euo pipefail

: "${EMSDK:?set EMSDK to an emsdk checkout with 3.1.50 activated}"
: "${WORK:?set WORK to a scratch directory}"
source "$EMSDK/emsdk_env.sh" >/dev/null

ZLIB_VERSION=1.3.1
GLIB_MINOR_VERSION=2.75
GLIB_VERSION=$GLIB_MINOR_VERSION.0
PIXMAN_VERSION=0.42.2
FFI_VERSION=adbcf2b247696dde2667ab552cb93e0c79455c84

export TARGET=$WORK/target
export CFLAGS="-O2 -matomics -mbulk-memory -DNDEBUG -sWASM_BIGINT -DWASM_BIGINT -pthread -sMALLOC=mimalloc -sASYNCIFY=1"
export CXXFLAGS="$CFLAGS"
export LDFLAGS="-L$TARGET/lib -O2"
export CPATH="$TARGET/include"
export PKG_CONFIG_PATH="$TARGET/lib/pkgconfig"
export EM_PKG_CONFIG_PATH="$PKG_CONFIG_PATH"
export CHOST=wasm32-unknown-linux
NPROC=$(nproc)
mkdir -p "$TARGET/lib" "$WORK/src"
cd "$WORK/src"

if [ ! -f "$TARGET/lib/libz.a" ]; then
  rm -rf zlib && mkdir zlib
  curl -Ls "https://zlib.net/fossils/zlib-$ZLIB_VERSION.tar.gz" | tar xzC zlib --strip-components=1
  (cd zlib && emconfigure ./configure --prefix="$TARGET" --static && make -j"$NPROC" install)
fi

if [ ! -f "$TARGET/include/ffi.h" ]; then
  rm -rf libffi && git clone -q https://github.com/libffi/libffi libffi
  (cd libffi && git checkout -q $FFI_VERSION && autoreconf -fiv >/dev/null &&
    LDFLAGS="$LDFLAGS -sEXPORTED_RUNTIME_METHODS=getTempRet0,setTempRet0" \
    emconfigure ./configure --host=$CHOST --prefix="$TARGET" --enable-static --disable-shared \
      --disable-dependency-tracking --disable-builddir --disable-multi-os-directory \
      --disable-raw-api --disable-structs --disable-docs &&
    emmake make install SUBDIRS=include)
fi

# Emscripten ignores some -s flags at compile time with a warning, which meson's
# checks treat as errors.
cat > "$WORK/emcc-meson-wrap.sh" <<'EOT'
#!/bin/bash
cmd="$1"; shift
args=()
for a in "$@"; do args+=("${a//-Werror=unused-command-line-argument/-Wno-error=unused-command-line-argument}"); done
exec "$cmd" "${args[@]}"
EOT
chmod +x "$WORK/emcc-meson-wrap.sh"
cat > "$WORK/cross.meson" <<EOT
[host_machine]
system = 'emscripten'
cpu_family = 'wasm32'
cpu = 'wasm32'
endian = 'little'

[binaries]
c = ['bash', '$WORK/emcc-meson-wrap.sh', 'emcc']
cpp = ['bash', '$WORK/emcc-meson-wrap.sh', 'em++']
ar = 'emar'
ranlib = 'emranlib'
pkgconfig = ['pkg-config', '--static']
EOT

if [ ! -f "$TARGET/lib/libglib-2.0.a" ]; then
  printf '#include <netdb.h>\nint res_query(const char *n, int c, int t, unsigned char *d, int l) { h_errno = HOST_NOT_FOUND; return -1; }\n' > res_query.c
  emcc $CFLAGS -c res_query.c -fPIC -o libresolv.o
  emar rcs "$TARGET/lib/libresolv.a" libresolv.o
  rm -rf glib && mkdir glib
  curl -Lks "https://download.gnome.org/sources/glib/$GLIB_MINOR_VERSION/glib-$GLIB_VERSION.tar.xz" | tar xJC glib --strip-components=1
  (cd glib &&
    CFLAGS="-Wno-error=incompatible-function-pointer-types -Wincompatible-function-pointer-types $CFLAGS" \
    meson setup _build --prefix="$TARGET" --cross-file="$WORK/cross.meson" --default-library=static \
      --buildtype=release --force-fallback-for=pcre2,gvdb -Dselinux=disabled -Dxattr=false \
      -Dlibmount=disabled -Dnls=disabled -Dtests=false -Dglib_assert=false -Dglib_checks=false &&
    for d in HAVE_CLOSE_RANGE HAVE_EPOLL_CREATE HAVE_KQUEUE HAVE_POSIX_SPAWN HAVE_FALLOCATE; do
      sed -i -E "/#define $d 1/d" _build/config.h
    done &&
    meson install -C _build)
fi

if [ ! -f "$TARGET/lib/libpixman-1.a" ]; then
  rm -rf pixman && git clone -q https://gitlab.freedesktop.org/pixman/pixman pixman
  (cd pixman && git checkout -q pixman-$PIXMAN_VERSION && NOCONFIGURE=y ./autogen.sh &&
    emconfigure ./configure --prefix="$TARGET" && emmake make -j"$NPROC" && emmake make install)
  rm -f "$TARGET"/lib/libpixman-1.so*
fi
echo "deps ready in $TARGET"
