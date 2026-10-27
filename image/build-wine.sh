#!/bin/bash
# Build vnbox's Wine (same version, patches and configure options as vnbox's
# release, new WoW64) against the guest's Ubuntu 26.04 libraries.
#
#   VNBOX_BIN=path/to/vnbox image/build-wine.sh BUILDROOT
#
# BUILDROOT is a throwaway copy of the guest rootfs (build deps get installed
# into it). Result: BUILDROOT/opt/wine-vnbox, to be copied into the guest.
# vnbox's prebuilt release links Ubuntu 24.04's ffmpeg (libavcodec60), which
# 26.04 does not ship, so video playback needs this rebuild.
set -euo pipefail
B=${1:?}
: "${VNBOX_BIN:?}"
install -m 0755 "$VNBOX_BIN" "$B/usr/local/bin/vnbox"
cleanup() { umount "$B/proc" "$B/sys" "$B/dev" 2>/dev/null || true; }
trap cleanup EXIT
mount -t proc proc "$B/proc"; mount --bind /sys "$B/sys"; mount --bind /dev "$B/dev"
cp /etc/resolv.conf "$B/etc/resolv.conf" 2>/dev/null || true

chroot "$B" /bin/bash -euo pipefail <<'EOF'
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# Same list as vnbox's .github/workflows/wine.yml.
apt-get install -y -qq --no-install-recommends build-essential gcc-mingw-w64 flex bison gettext \
  libx11-dev libxext-dev libxrandr-dev libxi-dev libxcursor-dev libxinerama-dev \
  libxcomposite-dev libxfixes-dev libxrender-dev libxxf86vm-dev libxkbcommon-dev \
  libfreetype-dev libfontconfig-dev libgl-dev libegl-dev libosmesa6-dev libgbm-dev libdrm-dev \
  libvulkan-dev libwayland-dev libpulse-dev libasound2-dev libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev libgnutls28-dev libdbus-1-dev libunwind-dev libudev-dev \
  libusb-1.0-0-dev libsdl2-dev libcups2-dev libkrb5-dev libpcap-dev libv4l-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswresample-dev libpcsclite-dev ocl-icd-opencl-dev \
  xz-utils ca-certificates
mkdir -p /tmp/vnbox-build
VNBOX=/tmp/vnbox-build vnbox build-wine -prefix /opt/wine-vnbox
/opt/wine-vnbox/bin/wine --version
EOF
