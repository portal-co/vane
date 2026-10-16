#!/usr/bin/env bash
# Build the Linux initramfs from the Alpine riscv64 minirootfs:
# extract, install our /init test script, repack as a gzip'd newc cpio.
set -euo pipefail
cd "$(dirname "$0")/linux"

rootfs=alpine-minirootfs-20230329-riscv64.tar.gz
if [[ ! -f "$rootfs" ]]; then
  echo "fetch the Alpine riscv64 minirootfs first (see harness/linux/README.md)" >&2
  exit 1
fi

rm -rf rootfs initrd work
mkdir -p rootfs initrd
tar -xzf "$rootfs" -C rootfs

cat > rootfs/init <<'INITEOF'
#!/bin/busybox sh
export PATH=/bin:/sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
echo
echo "=== VANE LINUX BOOT OK ==="
/bin/busybox uname -a 2>&1 || echo "uname failed"
echo "=== /proc/cpuinfo ==="
/bin/busybox cat /proc/cpuinfo 2>&1 | /bin/busybox head -8 || echo "cpuinfo failed"
echo "=== run real programs ==="
echo "hello-from-busybox" | /bin/busybox sha256sum 2>&1 || echo "sha256sum failed"
/bin/busybox ls / 2>&1 || echo "ls failed"
echo "=== VANE TESTS DONE ==="
poweroff -f
INITEOF'
#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null
echo
echo "=== VANE LINUX BOOT OK ==="
uname -a
echo "=== /proc/cpuinfo ==="
cat /proc/cpuinfo | head -8
echo "=== run real programs ==="
echo "hello-from-busybox" | shasum
ls / | head
echo "=== VANE TESTS DONE ==="
poweroff -f
INITEOF
chmod +x rootfs/init

(cd rootfs && bsdtar --format newc -cf ../initrd.cpio .)
gzip -9 -f -c initrd.cpio > initrd.gz
rm -rf rootfs initrd.cpio
ls -la initrd.gz
