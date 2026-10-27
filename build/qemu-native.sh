#!/bin/bash
# Build native qemu-system-x86_64 from the same (patched) source as the wasm
# one. Only used to boot the guest once and snapshot it (image/snapshot.sh).
#
#   WORK=... QEMU_SRC=third_party/qemu-wasm build/qemu-native.sh
set -euo pipefail
: "${WORK:?}" "${QEMU_SRC:?}"
QEMU_SRC=$(realpath "$QEMU_SRC")
mkdir -p "$WORK/qemu-native"
cd "$WORK/qemu-native"
if [ ! -f build.ninja ]; then
  "$QEMU_SRC/configure" --target-list=x86_64-softmmu --without-default-features --enable-system \
    --with-coroutine=ucontext --enable-virtfs --enable-attr --enable-cap-ng --disable-docs --disable-tools
fi
make -j"$(nproc)" qemu-system-x86_64
