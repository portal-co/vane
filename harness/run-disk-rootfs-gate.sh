#!/usr/bin/env bash
# R8 disk-rootfs gate, initramfs-modules variant (PLAN_PI_IN_VM R8).
#
# Boots the stock Debian netboot kernel + the virtio-module initramfs
# (build-initramfs-virtio.sh) with a virtio-blk disk; the initramfs /init
# insmods the module set, mounts /dev/vda (ext4 driver; ext2 images work via
# CONFIG_EXT4_USE_FOR_EXT2=y), and switch_roots into the disk's /init.
#
# Usage: run-disk-rootfs-gate.sh [path-to-rootfs.img]
#   Default image: ../vane/harness/linux/rootfs-test.img if present
#   (built by the vane terminal's build-rootfs-ext2.py), else build a local
#   one with e2fsprogs if available.
#
# NOTE: interpreter-speed boot — tens of minutes. Manual/nightly gate.
# RAM is 512MiB: the 128MiB default OOMs the initramfs /init (kernel +
# unpacked initramfs + module set).
set -euo pipefail
cd "$(dirname "$0")/.."

artifacts=harness/linux
# The disk gates need the *generic* sid kernel (virtio_blk/ext4 modules
# available + version-matched); the netboot installer kernel does not work
# here (see harness/linux/README.md).
kernel="$artifacts/linux-generic"
[[ -f "$kernel" ]] || kernel="$artifacts/linux"
for f in opensbi.bin "$kernel"; do
  [[ -f "$f" ]] || { echo "missing $f — see harness/linux/README.md" >&2; exit 2; }
done
[[ -f "$artifacts/initrd-virtio.gz" ]] || { echo "run harness/build-initramfs-virtio.sh first" >&2; exit 2; }

img="${1:-}"
if [[ -z "$img" ]]; then
  for cand in ../vane/harness/linux/rootfs-test.img "$artifacts/rootfs-test.img"; do
    [[ -f "$cand" ]] && img="$cand" && break
  done
fi
[[ -n "$img" && -f "$img" ]] || {
  echo "no rootfs image found; pass one explicitly (build via the vane repo's" >&2
  echo "harness/build-rootfs-ext2.py or mkfs.ext4 from e2fsprogs)" >&2
  exit 2
}

echo "== building vane-cli ==" >&2
cargo build -q -p vane-cli --no-default-features

echo "== booting Linux with --disk $img (tens of minutes) ==" >&2
start=$(date +%s)
out="$(cargo run -q -p vane-cli --no-default-features -- \
  --machine --opensbi "$artifacts/opensbi.bin" --kernel "$kernel" \
  --initrd "$artifacts/initrd-virtio.gz" --disk "$img" \
  --ram-size 0x20000000 \
  --append "console=ttyS0 panic=-1 root=/dev/vda rw rootfstype=ext4 vane_epoch=$(date +%s)" 2>/dev/null)"
end=$(date +%s)

fail() { echo "FAIL: $1" >&2; exit 1; }
grep -q "=== VANE VIRTIO INITRAMFS ===" <<<"$out" || fail "initramfs init did not run"
grep -q "modules loaded" <<<"$out" || fail "module load failed"
grep -q "=== VANE DISK MOUNTED ===" <<<"$out" || fail "did not mount /dev/vda"
grep -q "vane-virtio-blk-rootfs" <<<"$out" || fail "gate-marker unreadable from disk"
grep -q "=== VANE DISK-ROOTFS" <<<"$out" || fail "no disk-rootfs marker"

mkdir -p harness/results
cat > "harness/results/$(date +%F)-disk-rootfs.json" <<JSON
{"stage":"disk-rootfs","started_at":$start,"wall_seconds":$((end-start)),"from_snapshot":null,"pass":true,"host_os":"$(uname -s)","runtime":"vane-cli native","notes":"initramfs modules + ext4(ext2) mount, R8"}
JSON
echo "PASS: disk-rootfs gate ($((end-start))s)" >&2
