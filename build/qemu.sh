#!/bin/bash
# Build qemu-system-x86_64 for the browser (emscripten, TCG -> wasm JIT).
#
#   EMSDK=... WORK=... QEMU_SRC=third_party/qemu-wasm build/qemu.sh
#
# Needs build/deps.sh first. Output: $WORK/qemu-build/qemu-system-x86_64{,.wasm,.worker.js}
set -euo pipefail

: "${EMSDK:?}" "${WORK:?}" "${QEMU_SRC:?}"
source "$EMSDK/emsdk_env.sh" >/dev/null
QEMU_SRC=$(realpath "$QEMU_SRC")
TARGET=$WORK/target
export CPATH="$TARGET/include"
export PKG_CONFIG_PATH="$TARGET/lib/pkgconfig"
export EM_PKG_CONFIG_PATH="$PKG_CONFIG_PATH"

# Guest RAM + JIT caches must fit in wasm32's 4GB; iOS/Android tabs die well
# below that, so the heap is fixed rather than growable.
MEM=${QEMU_WASM_MEMORY:-2600MB}
OPT=${QEMU_WASM_OPT:--O3}

EXTRA_CFLAGS="$OPT -g2 -Wno-error=unused-command-line-argument -matomics -mbulk-memory -msimd128 \
 -DNDEBUG -DG_DISABLE_ASSERT -D_GNU_SOURCE -sASYNCIFY=1 -pthread -sPROXY_TO_PTHREAD=1 \
 -sFORCE_FILESYSTEM -sALLOW_TABLE_GROWTH -sTOTAL_MEMORY=$MEM -sWASM_BIGINT -sMALLOC=mimalloc \
 -sEXPORT_ES6=1 -sASYNCIFY_IMPORTS=ffi_call_js -sEXIT_RUNTIME=0 -sENVIRONMENT=web,worker,node"
VNVM_ROOT=$(cd "$(dirname "$0")/.." && pwd)
# QEMU_WASM_ASSERT=1 links with emscripten assertions (stack traces for aborts).
# Asyncify exists only for QEMU's coroutines (util/coroutine-fiber.c), which
# run on the main loop thread. The vCPU thread never unwinds, so the code that
# only it executes (TCG, the x86 helpers, softfloat, cputlb) is left
# uninstrumented: $ASYNCIFY_REMOVE is generated from those objects below.
ASYNCIFY_REMOVE=$WORK/asyncify-remove.json
EXTRA_LDFLAGS="${QEMU_WASM_ASSERT:+-sASSERTIONS=1 }--pre-js=$VNVM_ROOT/web/qemu/vnvm-disk.js -sEXPORTED_RUNTIME_METHODS=getTempRet0,setTempRet0,addFunction,removeFunction,FS,HEAPU8,HEAP32 \
 -sASYNCIFY_REMOVE=@$ASYNCIFY_REMOVE -L$TARGET/lib"
[ -f "$ASYNCIFY_REMOVE" ] || echo '[]' > "$ASYNCIFY_REMOVE"

mkdir -p "$WORK/qemu-build"
cd "$WORK/qemu-build"
if [ ! -f build.ninja ] || [ "${RECONFIGURE:-0}" = 1 ]; then
  emconfigure "$QEMU_SRC/configure" --static --target-list=x86_64-softmmu --cpu=wasm32 --cross-prefix= \
    --without-default-features --enable-system --with-coroutine=fiber --enable-virtfs \
    --disable-docs --disable-tools \
    --extra-cflags="$EXTRA_CFLAGS" --extra-cxxflags="$EXTRA_CFLAGS" --extra-ldflags="$EXTRA_LDFLAGS"
fi
# Objects first, then the uninstrumented-function list from them, then the link.
emmake make -j"$(nproc)" libqemu-x86_64-softmmu.fa tcg/libtcg_system.fa
"$VNVM_ROOT/build/asyncify-remove.sh" "$WORK/qemu-build" > "$ASYNCIFY_REMOVE.new"
if ! cmp -s "$ASYNCIFY_REMOVE" "$ASYNCIFY_REMOVE.new"; then
  mv "$ASYNCIFY_REMOVE.new" "$ASYNCIFY_REMOVE"
  rm -f qemu-system-x86_64  # ninja does not know about the list file
fi
rm -f "$ASYNCIFY_REMOVE.new"
emmake make -j"$(nproc)" qemu-system-x86_64
ls -la qemu-system-x86_64*

# Install into the web app.
OUT=$VNVM_ROOT/web/public/qemu
mkdir -p "$OUT/bios"
cp qemu-system-x86_64 "$OUT/qemu-system-x86_64.js"
cp qemu-system-x86_64.wasm qemu-system-x86_64.worker.js "$OUT/"
for f in bios-256k.bin vgabios-stdvga.bin kvmvapic.bin linuxboot_dma.bin; do
  cp "$QEMU_SRC/pc-bios/$f" "$OUT/bios/"
done
# vp build copies public/ into dist/; keep a built dist current too.
if [ -d "$VNVM_ROOT/web/dist/qemu" ]; then rm -rf "$VNVM_ROOT/web/dist/qemu"; cp -r "$OUT" "$VNVM_ROOT/web/dist/qemu"; fi
echo "installed into $OUT"
