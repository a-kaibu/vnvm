#!/bin/bash
# Turn a rootfs directory into a raw ext4 disk image.
#
#   image/make-disk.sh ROOTFS OUT.img [SIZE]
#
# No journal: the browser keeps writes in an overlay anyway, and journaling
# costs emulated CPU on every write.
set -euo pipefail
ROOTFS=${1:?}; OUT=${2:?}; SIZE=${3:-4G}
rm -f "$OUT"
truncate -s "$SIZE" "$OUT"
mkfs.ext4 -q -F -L vnvm-root -O ^has_journal,^metadata_csum,^orphan_file -E lazy_itable_init=0,lazy_journal_init=0 \
  -d "$ROOTFS" "$OUT"
echo "$OUT: $(du -h --apparent-size "$OUT" | cut -f1) virtual, $(du -h "$OUT" | cut -f1) used"
