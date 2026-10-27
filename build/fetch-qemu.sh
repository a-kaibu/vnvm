#!/bin/bash
# Check out the QEMU fork the browser build is made from and apply our patch.
#
#   build/fetch-qemu.sh [DIR]      (default third_party/qemu-wasm)
#
# Base: ktock/qemu-wasm (QEMU 8.2 with a TCG->wasm backend) at the commit
# below. patches/qemu-wasm-vnvm.patch holds everything vnvm changes: the
# display/audio/block drivers for the browser, the futex main loop, the JIT
# changes (register locals, tail-call chaining, thresholds) and bug fixes.
set -euo pipefail
DIR=${1:-$(cd "$(dirname "$0")/.." && pwd)/third_party/qemu-wasm}
BASE=0ef7b4e
if [ ! -d "$DIR/.git" ]; then
  git clone https://github.com/ktock/qemu-wasm "$DIR"
fi
cd "$DIR"
git checkout -q "$BASE"
git apply --whitespace=nowarn "$(dirname "$0")/../patches/qemu-wasm-vnvm.patch" 2>/dev/null ||
git apply --whitespace=nowarn "$(cd "$(dirname "$0")/.." && pwd)/patches/qemu-wasm-vnvm.patch"
echo "qemu-wasm $BASE + vnvm patch in $DIR"
