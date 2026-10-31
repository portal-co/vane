#!/usr/bin/env bash
# Full e2e Linux gate: virtio 9p + disk + network, in JIT and no-JIT builds.
#
# ONE boot per engine runs all three device families from the e2e initramfs
# (see build-initramfs-e2e.sh):
#   - disk: mount /dev/vda (ext4-on-ext2), write + sync + read back a marker
#   - 9p:   mount hostshare, read the host's file, write one back (verified
#           HOST-side after the run)
#   - net:  eth0 up, ARP + ping the built-in responder (10.0.0.1)
#
# Resource bounds (so this never overloads the machine):
#   - guest RAM is capped (VANE_E2E_RAM_MB, default 128) via --ram-size
#   - engines run SEQUENTIALLY (never parallel), and the boot runs at nice 19
#     (it yields to everything else on the box; under load, boots stretch
#     proportionally — the timeouts are wall-clock ceilings, not quiet-
#     machine estimates)
#   - each boot has a hard wall-clock timeout (VANE_E2E_TIMEOUT_*), and a
#     hung boot's process is killed
#
# Engines: JIT = --features native-jit; no-JIT = plain interpreter. The
# interpreter boot is SLOW (tens of minutes); this is a manual/nightly gate.
#
# Usage: harness/run-linux-e2e.sh [engines...]   (default: "jit nojit")
set -euo pipefail
cd "$(dirname "$0")/.."

artifacts=harness/linux
engines="${*:-jit nojit}"
ram_bytes=$(( ${VANE_E2E_RAM_MB:-128} * 1024 * 1024 ))
timeout_jit=${VANE_E2E_TIMEOUT_JIT:-2700}
timeout_nojit=${VANE_E2E_TIMEOUT_NOJIT:-5400}

# NOTE: linux-generic (7.2.11) is REQUIRED here: the module set inside
# initrd-virtio.gz is built for exactly that release (version magic
# 7.2.11+deb14-riscv64) — the older `linux` netboot kernel (7.0.13) rejects
# every module.
for f in opensbi.bin linux-generic initrd-virtio.gz; do
  [[ -f "$artifacts/$f" ]] || { echo "missing $artifacts/$f — see harness/linux/README.md" >&2; exit 2; }
done

# ── Host-side fixture: the 9p share ─────────────────────────────────────────
share="$(mktemp -d /tmp/vane-e2e-share.XXXXXX)"
mkdir -p /tmp/vane-e2e-logs
trap 'rm -rf "$share"' EXIT
echo "vane-e2e-host-to-guest-$RANDOM" > "$share/from-host.txt"

# ── Disk image (contract: harness/run-disk-rootfs-gate.sh) ──────────────────
work="$(mktemp -d /tmp/vane-e2e-work.XXXXXX)"
# Boot logs are preserved across failures in /tmp/vane-e2e-logs (cleaned only
# on explicit re-runs older than a day — kept simple: the gate never deletes).
trap 'rm -rf "$share"' EXIT
echo "vane-e2e-gate-marker" > "$work/gate-marker"
disk="$work/rootfs.img"
python3 harness/build-rootfs-ext2.py "$disk" 16384 \
  --file /etc/gate-marker:100644:"$work/gate-marker"

# ── Initramfs ────────────────────────────────────────────────────────────────
echo "== building the e2e initramfs ==" >&2
harness/build-initramfs-e2e.sh >/dev/null

# ── Build both engines ONCE up front (a mid-run rebuild would skew timings) ─
echo "== building vane-cli (no-jit) ==" >&2
cargo build -q -p vane-cli --no-default-features
bin_nojit="target/debug/vane-cli"
echo "== building vane-cli (jit) ==" >&2
cargo build -q -p vane-cli --no-default-features --features native-jit
bin_jit="target/debug/vane-cli"

# run_engine <name> <bin> <timeout_s>
run_engine() {
  local name="$1" bin="$2" tmo="$3"
  echo
  echo "════ e2e engine=$name (ram=$((ram_bytes/1024/1024))MiB timeout=${tmo}s) ════" >&2
  local log="/tmp/vane-e2e-logs/log-$name.txt"
  rm -f "$log"
  nice -n 19 "$bin" --machine --opensbi "$artifacts/opensbi.bin" --kernel "$artifacts/linux-generic" \
    --initrd "$artifacts/initrd-e2e.gz" --disk "$disk" --share "$share" --net \
    --append "console=ttyS0 panic=-1" --ram-size "$ram_bytes" \
    >"$log" 2>&1 &
  local pid=$!
  local waited=0
  while kill -0 "$pid" 2>/dev/null; do
    if (( waited >= tmo )); then
      echo "TIMEOUT after ${tmo}s — killing (partial log kept at $log)" >&2
      kill -9 "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
      break
    fi
    sleep 10
    waited=$((waited + 10))
    if (( waited % 60 == 0 )); then
      echo "  ...booting (${waited}s, log $(wc -c < "$log" | tr -d ' ') bytes)" >&2
    fi
  done

  local failed=0
  check() { grep -q "$1" "$log" && { echo "  PASS: $1" >&2; } || { echo "  FAIL: $1" >&2; failed=1; }; }
  check "=== VANE E2E BOOT ==="
  check "=== E2E DISK MOUNT OK ==="
  check "=== E2E DISK RW OK ==="
  check "=== E2E 9P MOUNT OK ==="
  check "=== E2E 9P READ OK ==="
  check "=== E2E 9P WRITE OK ==="
  check "=== E2E NET PING OK ==="
  check "reboot: Power down"
  # Host-side proof of the 9p round trip: the guest's write landed on the host.
  if grep -q "guest-wrote-this" "$share/to-host.txt" 2>/dev/null; then
    echo "  PASS: 9p host-side readback" >&2
  else
    echo "  FAIL: 9p host-side readback" >&2
    failed=1
  fi
  if (( failed )); then
    echo "e2e engine=$name: FAIL (log: $log)" >&2
    return 1
  fi
  echo "e2e engine=$name: PASS" >&2
}

rc=0
for eng in $engines; do
  case "$eng" in
    jit)   run_engine jit   "$bin_jit"   "$timeout_jit"   || rc=1 ;;
    nojit) run_engine nojit "$bin_nojit" "$timeout_nojit" || rc=1 ;;
    *) echo "unknown engine '$eng' (use: jit, nojit)" >&2; exit 2 ;;
  esac
done

if (( rc == 0 )); then
  echo
  echo "Linux e2e gate ($engines): ALL PASS" >&2
else
  echo
  echo "Linux e2e gate ($engines): FAILURES" >&2
fi
exit "$rc"
