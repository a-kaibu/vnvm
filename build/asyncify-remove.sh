#!/bin/bash
# Print (as a JSON list) the functions defined by the objects only the vCPU
# thread runs: target/i386, accel/tcg, fpu and tcg. build/qemu.sh passes the
# list as -sASYNCIFY_REMOVE so they are not Asyncify-instrumented (see there).
#
#   build/asyncify-remove.sh QEMU_BUILD_DIR
set -euo pipefail
B=${1:?}
NM=${NM:-$EMSDK/upstream/bin/llvm-nm}
cd "$B"
for o in libqemu-x86_64-softmmu.fa.p/target_i386_*.o libqemu-x86_64-softmmu.fa.p/accel_tcg_*.o \
         libqemu-x86_64-softmmu.fa.p/fpu_*.o tcg/libtcg_system.fa.p/*.o; do
  "$NM" --defined-only "$o" | awk '$2 ~ /^[Tt]$/ { print $3 }'
done | sort -u | python3 -c 'import json, sys; print(json.dumps(sys.stdin.read().split()))'
