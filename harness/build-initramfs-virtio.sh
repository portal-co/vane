#!/usr/bin/env bash
# Build the virtio-module initramfs for disk-rootfs gates (PLAN_PI_IN_VM R8).
#
# The Debian riscv64 kernel has virtio_mmio/virtio_blk/virtio_net/9p/ext4 as
# modules, so the disk-rootfs boot needs an initramfs that loads them before
# mounting the real root. This packs: Alpine busybox minirootfs + the module
# set (fetch-virtio-modules.sh) + an /init that loads modules and mounts the
# virtio-blk disk.
#
# With an initramfs, the kernel never performs the root= mount itself — the
# initramfs /init replaces it, so the load+mount happens here.
set -euo pipefail
cd "$(dirname "$0")/linux"

rootfs=alpine-minirootfs-20230329-riscv64.tar.gz
[[ -f "$rootfs" ]] || { echo "missing harness/linux/$rootfs — see harness/linux/README.md" >&2; exit 1; }
[[ -f modules/virtio_blk.ko ]] || { echo "run fetch-virtio-modules.sh first" >&2; exit 1; }

rm -rf rootfs-virtio
mkdir -p rootfs-virtio
tar -xzf "$rootfs" -C rootfs-virtio

mkdir -p rootfs-virtio/lib/modules
cp modules/*.ko rootfs-virtio/lib/modules/
cp modules/load-modules.sh rootfs-virtio/lib/

cat > rootfs-virtio/init <<'INITEOF'
#!/bin/busybox sh
# Disk-rootfs gate init (R8): load the virtio/ext4 module set from the
# initramfs, mount the virtio-blk disk, run its /init (or verify directly).
export PATH=/bin:/sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null

echo "=== VANE VIRTIO INITRAMFS ==="
sh /lib/load-modules.sh || { echo "FAIL: module load"; poweroff -f; }
echo "modules loaded"

# R7: guest clock from the host cmdline.
_epoch=$(sed -n 's/.*vane_epoch=\([0-9]*\).*/\1/p' /proc/cmdline)
[ -n "$_epoch" ] && date -s "@$_epoch" 2>/dev/null || true

mkdir -p /newroot
# EXT4_USE_FOR_EXT2=y in this kernel: ext4 mounts ext2 images too.
mount -t ext4 -o rw /dev/vda /newroot || { echo "FAIL: mount /dev/vda"; poweroff -f; }
echo "=== VANE DISK MOUNTED ==="
cat /newroot/etc/gate-marker 2>/dev/null || true

if [ -x /newroot/init ]; then
  echo "=== VANE DISK-ROOTFS HANDOFF ==="
  exec switch_root /newroot /init
fi
echo "=== VANE DISK-ROOTFS OK ==="
poweroff -f
INITEOF
chmod +x rootfs-virtio/init

(cd rootfs-virtio && bsdtar --format newc -cf ../initrd-virtio.cpio .)
gzip -9 -f -c initrd-virtio.cpio > initrd-virtio.gz
rm -rf rootfs-virtio initrd-virtio.cpio
ls -la initrd-virtio.gz
