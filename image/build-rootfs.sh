#!/bin/bash
# Build the guest: stock Ubuntu 26.04 (resolute) amd64 + Xorg. vnbox's Wine
# goes in afterwards with image/install-wine.sh.
#
#   image/build-rootfs.sh OUT_DIR
#
# Produces OUT_DIR/rootfs (a directory). image/make-disk.sh turns it into the
# disk image. Packages are unmodified Ubuntu; only configuration is ours.
set -euo pipefail

OUT=${1:?usage: build-rootfs.sh OUT_DIR}
HERE=$(cd "$(dirname "$0")" && pwd)
R=$OUT/rootfs
MIRROR=${MIRROR:-http://archive.ubuntu.com/ubuntu}

[ -e /usr/share/debootstrap/scripts/resolute ] || ln -s gutsy /usr/share/debootstrap/scripts/resolute

if [ ! -f "$R/var/lib/dpkg/status" ]; then
  debootstrap --arch=amd64 --variant=minbase --components=main,universe \
    --include=systemd-sysv,udev,kmod,linux-image-virtual,xserver-xorg-core,xinit,x11-xserver-utils,fonts-noto-cjk,ca-certificates,procps,locales,dbus,iproute2,alsa-utils,squashfs-tools \
    resolute "$R" "$MIRROR"
fi

cleanup() { umount "$R/proc" "$R/sys" "$R/dev" 2>/dev/null || true; }
trap cleanup EXIT
mount -t proc proc "$R/proc"; mount --bind /sys "$R/sys"; mount --bind /dev "$R/dev"

cat > "$R/etc/apt/sources.list.d/ubuntu.sources" <<EOF
Types: deb
Architectures: amd64
URIs: $MIRROR
Suites: resolute resolute-updates
Components: main universe
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
rm -f "$R/etc/apt/sources.list"

chroot "$R" /bin/bash -euo pipefail <<'EOF'
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends xserver-xorg-video-modesetting x11-utils
sed -i 's/^# *ja_JP.UTF-8/ja_JP.UTF-8/' /etc/locale.gen
grep -q '^ja_JP.UTF-8' /etc/locale.gen || echo 'ja_JP.UTF-8 UTF-8' >> /etc/locale.gen
locale-gen >/dev/null
echo 'LANG=ja_JP.UTF-8' > /etc/default/locale
echo vnvm > /etc/hostname
passwd -d root
apt-get clean
rm -rf /var/lib/apt/lists/*
EOF

# --- configuration (everything below is ours) ---
install -m 0644 "$HERE/files/fstab" "$R/etc/fstab"
install -m 0644 "$HERE/files/xorg.conf" "$R/etc/X11/xorg.conf"
install -m 0644 "$HERE/files/asound.conf" "$R/etc/asound.conf"
install -m 0755 "$HERE/files/vnvm-session" "$HERE/files/vnvm-bench" "$R/usr/local/bin/"
install -m 0644 "$HERE/files/vnvm-session.service" "$R/etc/systemd/system/vnvm-session.service"
install -m 0644 "$HERE/files/journald.conf" "$R/etc/systemd/journald.conf.d/vnvm.conf" -D
install -m 0644 "$HERE/files/modules" "$R/etc/modules-load.d/vnvm.conf"
mkdir -p "$R/game" "$R/save"

chroot "$R" /bin/bash -euo pipefail <<'EOF'
systemctl enable vnvm-session.service
# Nothing periodic or networked: every wakeup costs emulated cycles.
for u in apt-daily.timer apt-daily-upgrade.timer dpkg-db-backup.timer e2scrub_all.timer \
         e2scrub_reap.service fstrim.timer motd-news.timer systemd-pstore.service \
         systemd-networkd.service systemd-networkd-wait-online.service systemd-timesyncd.service \
         systemd-tmpfiles-clean.timer getty@tty1.service; do
  systemctl mask "$u" >/dev/null 2>&1 || true
done
systemctl set-default multi-user.target
EOF
echo "rootfs ready: $R"
