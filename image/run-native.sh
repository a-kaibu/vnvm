#!/bin/bash
# Boot the guest under native QEMU (TCG, one vCPU) with exactly the device
# set the browser uses, so its state can be snapshotted and resumed there.
#
#   QEMU=path/to/qemu-system-x86_64 image/run-native.sh DIR [extra qemu args...]
#
# DIR holds vmlinuz, root.img, game.img, save.img. Serial goes to DIR/serial.log,
# the HMP monitor to DIR/monitor.sock.
set -euo pipefail
DIR=${1:?}; shift
QEMU=${QEMU:-qemu-system-x86_64}
exec "$QEMU" $(cat "$(dirname "$0")/qemu-args.txt") \
  -display none -serial "file:$DIR/serial.log" -monitor "unix:$DIR/monitor.sock,server,nowait" \
  -kernel "$DIR/vmlinuz" -append "${KCMD:-$(cat "$(dirname "$0")/kernel-cmdline.txt")}" \
  -audiodev none,id=snd0 \
  -drive "if=none,id=root,format=raw,file=$DIR/root.img" \
  -drive "if=none,id=game,format=raw,file=$DIR/game.img" \
  -drive "if=none,id=save,format=raw,file=$DIR/save.img" \
  "$@"
