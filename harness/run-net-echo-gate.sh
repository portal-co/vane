#!/usr/bin/env bash
# Net echo gate (PLAN_PI_IN_VM P1): boots Linux with --net (virtio-net +
# smoltcp tunnel) and requires the guest to receive the host echo server's
# banner and an echoed line back through the tunnel.
#
# Offline-safe: the guest connects to a static synthetic mapping
# (10.0.2.100 -> 127.0.0.1) served by harness/net/echo_server.py.
#
# NOTE: interpreter-speed boot — expect tens of minutes (see
# run-linux-boot-gate.sh). Manual/nightly gate, not CI.
set -euo pipefail
cd "$(dirname "$0")/.."

artifacts=harness/linux
for f in opensbi.bin linux; do
  [[ -f "$artifacts/$f" ]] || {
    echo "missing $artifacts/$f — see harness/linux/README.md" >&2
    exit 2
  }
done

# Build the net-gate initramfs (base minirootfs + guest-init.sh as /init).
rootfs_tgz="$artifacts/alpine-minirootfs-20230329-riscv64.tar.gz"
[[ -f "$rootfs_tgz" ]] || { echo "missing $rootfs_tgz" >&2; exit 2; }
work="$(mktemp -d)"
trap 'rm -rf "$work"; [[ -n "${srv_pid:-}" ]] && kill "$srv_pid" 2>/dev/null || true' EXIT
mkdir -p "$work/rootfs"
tar -xzf "$rootfs_tgz" -C "$work/rootfs"
cp harness/net/guest-init.sh "$work/rootfs/init"
chmod +x "$work/rootfs/init"
(cd "$work/rootfs" && bsdtar --format newc -cf "$work/initrd.cpio" .)
gzip -9 -f -c "$work/initrd.cpio" > "$work/initrd.gz"

# Host echo server.
python3 harness/net/echo_server.py 17777 > "$work/srv.log" 2>&1 &
srv_pid=$!
sleep 1
grep -q "PORT 17777" "$work/srv.log" || { echo "FAIL: echo server did not start" >&2; exit 1; }

echo "== building vane-cli ==" >&2
cargo build -q -p vane-cli --no-default-features

echo "== booting Linux with --net (this takes a while) ==" >&2
start=$(date +%s)
out="$(cargo run -q -p vane-cli --no-default-features -- \
  --machine --opensbi "$artifacts/opensbi.bin" --kernel "$artifacts/linux" \
  --initrd "$work/initrd.gz" --append "console=ttyS0 panic=-1" \
  --net --net-map 10.0.2.100=127.0.0.1 2>/dev/null)"
end=$(date +%s)

fail() { echo "FAIL: $1" >&2; exit 1; }
grep -q "=== VANE NET GATE ===" <<<"$out" || fail "guest init did not run"
grep -q "VANE-ECHO-BANNER" <<<"$out" || fail "no banner through tunnel"
grep -q "=== VANE NET ECHO OK ===" <<<"$out" || fail "echo did not round-trip"

mkdir -p harness/results
cat > "harness/results/$(date +%F)-net-echo.json" <<JSON
{"stage":"net-echo","started_at":$start,"wall_seconds":$((end-start)),"from_snapshot":null,"pass":true,"host_os":"$(uname -s)","runtime":"vane-cli native","notes":"static map 10.0.2.100=127.0.0.1"}
JSON
echo "PASS: net echo gate ($((end-start))s)" >&2
