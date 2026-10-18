#!/usr/bin/env bash
# R8 disk-rootfs gate (PLAN_PI_IN_VM.md §9/§13; PLAN_VIRTIO_WEVAL V2).
#
# SUPERSEDED as a runnable gate by t-26ef's initramfs-based version on the
# vane-agent branch — kept here as the CONTRACT reference (the image format and
# the marker assertions). Rationale: the Debian netboot kernel ships
# VIRTIO_MMIO/VIRTIO_BLK as MODULES (and EXT2_FS is unset), so a direct
# root=/dev/vda boot fails with unknown-block(0,0). The working flow boots the
# netboot kernel + an initramfs that insmods the module set, mounts the ext2
# image via the ext4 driver (EXT4_USE_FOR_EXT2=y), and switch_roots to the
# disk /init.
#
# Contract this defines:
#   - image: partitionless ext2 (build-rootfs-ext2.py), /init + /etc/gate-marker
#     + /bin/busybox; mounts via rootfstype=ext4 (ext4 handles ext2)
#   - cmdline: root=/dev/vda rw rootfstype=ext4 console=ttyS0 panic=-1
#   - markers: "VFS: Mounted root" / the switch_root target, the
#     "=== VANE DISK-ROOTFS OK ===" marker, the "vane-virtio-blk-rootfs"
#     gate-marker readback, and a clean poweroff
#
# NOTE: the interpreter is slow; a full boot to /init takes tens of minutes.
# This is a manual/nightly gate, not a per-commit CI job.
set -euo pipefail
cd "$(dirname "$0")/.."

artifacts=harness/linux
for f in opensbi.bin linux alpine-minirootfs-20230329-riscv64.tar.gz; do
  [[ -f "$artifacts/$f" ]] || { echo "missing $artifacts/$f" >&2; exit 2; }
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "== extracting busybox ==" >&2
tar -xzf "$artifacts/alpine-minirootfs-20230329-riscv64.tar.gz" -C "$work" bin/busybox

echo "== building the ext2 rootfs image ==" >&2
cat > "$work/init" <<'EOF'
#!/bin/busybox sh
export PATH=/bin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
echo "=== VANE DISK-ROOTFS OK ==="
/bin/busybox cat /etc/gate-marker
poweroff -f
EOF
echo "vane-virtio-blk-rootfs" > "$work/gate-marker"
img="$work/rootfs.img"
python3 harness/build-rootfs-ext2.py "$img" 16384 \
  --file /init:100755:"$work/init" \
  --file /bin/busybox:100755:"$work/bin/busybox" \
  --file /etc/gate-marker:100644:"$work/gate-marker"

echo "== building vane-cli ==" >&2
cargo build -q -p vane-cli --no-default-features

echo "== booting Linux with disk rootfs (slow) ==" >&2
out="$(cargo run -q -p vane-cli --no-default-features -- \
  --machine --opensbi "$artifacts/opensbi.bin" --kernel "$artifacts/linux" \
  --disk "$img" \
  --append "root=/dev/vda rw rootfstype=ext2 console=ttyS0 panic=-1" 2>/dev/null)"

fail() { echo "FAIL: $1" >&2; exit 1; }
grep -q "Platform Name               : vane-virt" <<<"$out" || fail "no OpenSBI banner"
grep -qi "virtio_blk\|virtio-blk" <<<"$out" || fail "virtio_blk did not probe (is it a module?)"
grep -q "VFS: Mounted root" <<<"$out" || fail "root fs not mounted"
grep -q "=== VANE DISK-ROOTFS OK ===" <<<"$out" || fail "no /init marker"
grep -q "vane-virtio-blk-rootfs" <<<"$out" || fail "gate-marker not read back"
grep -q "reboot: Power down" <<<"$out" || fail "no clean poweroff"

echo "R8 disk-rootfs gate: PASS" >&2
