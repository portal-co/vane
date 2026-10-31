#!/usr/bin/env bash
# Build the e2e initramfs (PLAN_LINUX_WASM follow-up: virtio 9p + disk + net):
# take the existing initrd-virtio.gz (busybox + the Debian module set:
# virtio_mmio/blk/net, ext4 stack, 9p/9pnet/9pnet_virtio), swap /init for the
# e2e test script, repack.
#
# The e2e init exercises, in one boot:
#   - virtio-blk: mount /dev/vda, write + sync + read back a marker
#   - virtio-9p:  mount hostshare, read the host's from-host.txt, write
#                 to-host.txt back to the host (the gate checks it host-side)
#   - virtio-net: bring up eth0 (10.0.0.2) and ping the built-in ARP/ICMP
#                 responder peer (10.0.0.1, vane-cli --net)
set -euo pipefail
BASE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$BASE/harness/linux"

src=initrd-virtio.gz
[[ -f "$src" ]] || { echo "missing $src" >&2; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "== extracting the virtio initramfs (modules + busybox) ==" >&2
gzip -dc "$src" > "$work/base.cpio"
mkdir -p "$work/rootfs"
(cd "$work/rootfs" && cpio -idm --quiet < "$work/base.cpio")
[[ -f "$work/rootfs/bin/busybox" ]] || {
  echo "initrd-virtio.gz has no /bin/busybox" >&2
  exit 1
}

echo "== adding the extra modules (netfs/failover/net_failover) ==" >&2
"$BASE/harness/fetch-modules.sh" >/dev/null
mkdir -p "$work/rootfs/lib/modules"
cp "$BASE"/harness/linux/modules-extra/*.ko "$work/rootfs/lib/modules/"

echo "== installing the e2e /init ==" >&2
cat > "$work/rootfs/init" <<'INITEOF'
#!/bin/busybox sh
# e2e init: exercise virtio-blk (disk), virtio-9p (host share), virtio-net
# (ping the built-in ARP/ICMP responder). One boot, three device families.
export PATH=/bin:/sbin
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev 2>/dev/null

echo "=== VANE E2E BOOT ==="
# Dependency order proven on the R8 boot (deps before dependents), then the
# 9p + net sets (netfs underlies 9pnet; failover underlies net_failover
# underlies virtio_net).
for m in crc32c-cryptoapi crc16 virtio_mmio virtio_blk mbcache jbd2 ext4 \
         netfs 9pnet 9pnet_virtio 9p failover net_failover virtio_net; do
  insmod "/lib/modules/$m.ko" 2>/dev/null || echo "WARN: insmod $m"
done
echo "=== E2E MODULES DONE ==="

# ── DISK (virtio-blk) ────────────────────────────────────────────────────────
mkdir -p /mnt/disk
if mount -t ext4 -o rw /dev/vda /mnt/disk; then
  echo "=== E2E DISK MOUNT OK ==="
  echo "vane-e2e-disk-$$" > /mnt/disk/e2e-marker
  sync
  if grep -q "vane-e2e-disk" /mnt/disk/e2e-marker; then
    echo "=== E2E DISK RW OK ==="
  else
    echo "=== E2E DISK RW FAIL ==="
  fi
  cat /mnt/disk/etc/gate-marker 2>/dev/null || true
else
  echo "=== E2E DISK MOUNT FAIL ==="
fi

# ── 9P (virtio-9p host share) ────────────────────────────────────────────────
mkdir -p /mnt/host
if mount -t 9p -o trans=virtio,version=9p2000.L,msize=8192 hostshare /mnt/host; then
  echo "=== E2E 9P MOUNT OK ==="
  if cat /mnt/host/from-host.txt; then
    echo "=== E2E 9P READ OK ==="
  else
    echo "=== E2E 9P READ FAIL ==="
  fi
  echo "guest-wrote-this-$$" > /mnt/host/to-host.txt
  sync
  if grep -q "guest-wrote-this" /mnt/host/to-host.txt; then
    echo "=== E2E 9P WRITE OK ==="
  else
    echo "=== E2E 9P WRITE FAIL ==="
  fi
else
  echo "=== E2E 9P MOUNT FAIL ==="
fi

# ── NET (virtio-net: ARP for, then ping, the responder at 10.0.0.1) ─────────
ip link set eth0 up 2>/dev/null || ifconfig eth0 up
ip addr add 10.0.0.2/24 dev eth0 2>/dev/null \
  || ifconfig eth0 10.0.0.2 netmask 255.255.255.0 up
if ping -c 2 -W 2 10.0.0.1; then
  echo "=== E2E NET PING OK ==="
else
  echo "=== E2E NET PING FAIL ==="
fi

echo "=== VANE E2E ALL DONE ==="
poweroff -f
INITEOF
chmod +x "$work/rootfs/init"

echo "== repacking ==" >&2
(cd "$work/rootfs" && bsdtar --format newc -cf ../e2e.cpio .)
gzip -9 -f -c "$work/e2e.cpio" > initrd-e2e.gz
ls -la initrd-e2e.gz
