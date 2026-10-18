#!/usr/bin/env bash
# Fetch the virtio/ext4/9p kernel modules for the disk-rootfs gates
# (PLAN_PI_IN_VM R8). The Debian riscv64 kernel builds all of these as
# modules (verified against linux-config-7.2): VIRTIO_MMIO=m, VIRTIO_BLK=m,
# VIRTIO_NET=m, 9P*=m, EXT4_FS=m, CRYPTO_CRC32C=m (CONFIG_VIRTIO=y builtin).
# EXT2_FS is unset — disk images must be ext4 (or ext2 mounted via the ext4
# driver, EXT4_USE_FOR_EXT2=y).
#
# The module set must match the kernel's VERSION MAGIC exactly (insmod
# rejects otherwise). The magic is read from the kernel image itself
# (./linux), and the matching linux-modules-* package is located via the
# sid Packages index.
#
# Output: harness/linux/modules/*.ko (decompressed) + load-modules.sh.
# Not committed.
set -euo pipefail
cd "$(dirname "$0")"

kernel="${1:-./linux}"
[[ -f "$kernel" ]] || { echo "missing kernel image $kernel" >&2; exit 1; }

# NOTE: grep -m1 SIGPIPEs `strings` (141) — harmless; swallow so pipefail
# doesn't kill the script.
vermagic="$(strings -a "$kernel" | grep -m1 -o 'Linux version [^ ]*' | sed 's/Linux version //' || true)"
[[ -n "$vermagic" ]] || { echo "could not read version magic from $kernel" >&2; exit 1; }
echo "kernel version magic: $vermagic" >&2

pkg="linux-modules-${vermagic}"

echo "== locating $pkg in the sid riscv64 index ==" >&2
pkgfile="$(curl -fsL --max-time 300 "http://deb.debian.org/debian/dists/sid/main/binary-riscv64/Packages.gz" | gzip -dc)"
filename="$(awk -v p="Package: $pkg" '$0==p{f=1} f&&/^Filename:/{print $2; exit}' <<<"$pkgfile")"
[[ -n "$filename" ]] || { echo "package $pkg not in sid riscv64 (kernel too old? dists moved it to snapshot.debian.org)" >&2; exit 1; }

deb="${filename##*/}"
if [[ ! -f "$deb" ]]; then
  echo "== fetching $deb ==" >&2
  curl -fL --max-time 900 -o "$deb" "http://deb.debian.org/debian/$filename"
fi

# Dependency order for insmod (paths relative to .../kernel/).
WANT=(
  crypto/crc32c-cryptoapi.ko.xz        # ext4 metadata checksums
  drivers/virtio/virtio_mmio.ko.xz     # transport
  drivers/block/virtio_blk.ko.xz       # disk
  fs/mbcache.ko.xz fs/jbd2/jbd2.ko.xz fs/ext4/ext4.ko.xz
  drivers/net/virtio_net.ko.xz         # net gate
  net/9p/9pnet.ko.xz net/9p/9pnet_virtio.ko.xz fs/9p/9p.ko.xz  # host share
)

mkdir -p modules
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
ar p "$deb" data.tar.xz > "$tmp/data.tar.xz"

KDIR="usr/lib/modules/${vermagic}/kernel"
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
echo "modules for $vermagic ready in harness/linux/modules/" >&2
