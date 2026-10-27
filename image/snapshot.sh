#!/bin/bash
# Boot the guest under native QEMU until vnvm-session reports READY, then save
# the whole VM state. The browser resumes it with -incoming instead of booting
# (minutes of emulated systemd/Xorg/Wine startup become a download).
#
#   QEMU=path/to/native/qemu-system-x86_64 image/snapshot.sh DIR
#
# DIR as for run-native.sh; writes DIR/vm.state and modifies DIR/root.img (the
# guest writes during boot), so pack root.img only after this, together with
# vm.state: the snapshot's inode caches describe exactly that image. The
# native QEMU must be built from the same source as the wasm one
# (build/qemu-native.sh): the migration stream is only compatible between
# identical device models.
set -euo pipefail
DIR=$(realpath "${1:?}")
HERE=$(cd "$(dirname "$0")" && pwd)
rm -f "$DIR/serial.log" "$DIR/vm.state" "$DIR/monitor.sock"

"$HERE/run-native.sh" "$DIR" &
QPID=$!
trap 'kill $QPID 2>/dev/null || true' EXIT

t0=$(date +%s)
until grep -q "VNVM: READY" "$DIR/serial.log" 2>/dev/null; do
  sleep 1
  kill -0 $QPID 2>/dev/null || { echo "qemu exited"; exit 1; }
  if [ $(( $(date +%s) - t0 )) -gt 900 ]; then echo "timeout"; tail "$DIR/serial.log"; exit 1; fi
done
echo "guest ready after $(( $(date +%s) - t0 ))s"
# Let the session settle (wineserver idle, caches dropped).
sleep 5

python3 - "$DIR/monitor.sock" "$DIR/vm.state" <<'EOF'
import socket, sys, time
sock, out = sys.argv[1], sys.argv[2]
s = socket.socket(socket.AF_UNIX)
s.connect(sock)
def cmd(c, wait=0.3):
    s.sendall((c + "\n").encode())
    time.sleep(wait)
    try:
        s.setblocking(False)
        return s.recv(65536).decode(errors="replace")
    except BlockingIOError:
        return ""
    finally:
        s.setblocking(True)
cmd("")
cmd("stop")
cmd(f"migrate file:{out}")
while "completed" not in (r := cmd("info migrate", 1.0)):
    if "failed" in r:
        print(r); sys.exit(1)
print(r.split("\n")[0:6])
cmd("quit")
EOF
wait $QPID || true
ls -la "$DIR/vm.state"
