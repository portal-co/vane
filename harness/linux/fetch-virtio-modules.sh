#!/usr/bin/env bash
# Fetch the virtio/ext4/9p kernel modules for the disk-rootfs gates
# (PLAN_PI_IN_VM R8). The Debian riscv64 kernel (netboot AND cloud) builds
# all of these as modules — verified against linux-config-7.2
# (7.2.11-1): VIRTIO_MMIO=m, VIRTIO_BLK=m, VIRTIO_NET=m, 9P*=m, EXT4_FS=m,
# CRYPTO_CRC32C=m (CONFIG_VIRTIO=y is builtin). EXT2_FS is NOT SET — disk
# images must be ext4.
#
# Output: harness/linux/modules/*.ko (decompressed), ready to be packed
# into the gate initramfs. Not committed.
set -euo pipefail
cd "$(dirname "$0")"

DEB_VERSION="7.2.11-1"
DEB_NAME="linux-modules-7.2.11+deb14-riscv64_${DEB_VERSION}_riscv64.deb"
DEB_URL="http://deb.debian.org/debian/pool/main/l/linux/${DEB_NAME}"

# Dependency order for insmod.
WANT=(
  crypto/crc32c-cryptoapi.ko.xz        # ext4 metadata checksums
  drivers/virtio/virtio_mmio.ko.xz     # transport
  drivers/block/virtio_blk.ko.xz       # disk
  fs/mbcache.ko.xz fs/jbd2/jbd2.ko.xz fs/ext4/ext4.ko.xz
  drivers/net/virtio_net.ko.xz         # net gate
  net/9p/9pnet.ko.xz net/9p/9pnet_virtio.ko.xz fs/9p/9p.ko.xz  # host share
)

if [[ ! -f "$DEB_NAME" ]]; then
  echo "== fetching $DEB_NAME ==" >&2
  curl -fL --max-time 900 -o "$DEB_NAME" "$DEB_URL"
fi

mkdir -p modules
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
ar p "$DEB_NAME" data.tar.xz > "$tmp/data.tar.xz"

KDIR="usr/lib/modules/7.2.11+deb14-riscv64/kernel"
for m in "${WANT[@]}"; do
  tar -xJf "$tmp/data.tar.xz" -C "$tmp" "./$KDIR/$m"
  out="modules/$(basename "${m%.xz}")"
  xz -dc "$tmp/$KDIR/$m" > "$out"
  rm -f "$tmp/$KDIR/$m"
  echo "  $out" >&2
done

# In-guest loader fragment (packed at /lib/load-modules.sh in the initramfs).
cat > modules/load-modules.sh <<'EOF'
#!/bin/busybox sh
# Insmod the virtio/ext4/9p set in dependency order (PLAN_PI_IN_VM R8).
# The Debian riscv64 kernel builds all of these as modules.
set -e
cd /lib/modules
insmod crc32c-cryptoapi.ko
insmod virtio_mmio.ko
insmod virtio_blk.ko
insmod mbcache.ko
insmod jbd2.ko
insmod ext4.ko
insmod virtio_net.ko
insmod 9pnet.ko
insmod 9pnet_virtio.ko
insmod 9p.ko
EOF
echo "modules ready in harness/linux/modules/" >&2
