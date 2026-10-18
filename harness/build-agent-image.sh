#!/usr/bin/env bash
# Build the agent disk image (PLAN_PI_IN_VM M7): a glibc Debian riscv64 disk
# with Node.js (pinned in harness/node/VERSIONS), Pi (@earendil-works/
# pi-coding-agent), and the github:gkgoat1/pi-surplus-intelligence extension
# pre-installed.
#
# Method: boot the Debian sid "nocloud" riscv64 cloud image under Homebrew
# QEMU (software emulation, no HVF — see global AGENTS.md), with a NoCloud
# seed ISO whose runcmd provisions everything from a payload drive, then
# powers off. All runtime paths are ephemeral (mktemp); networking is QEMU
# user-mode (outbound NAT, needed for npm/apt); a bounded timeout applies;
# the serial console log is preserved under harness/results/ for diagnosis.
# No credentials are installed into the image (PLAN_PI_IN_VM §11 Sec-1).
#
# Prerequisites (fail-closed):
#   - qemu-system-riscv64, qemu-img (brew install qemu)
#   - harness/guest/debian-sid-nocloud-riscv64.qcow2 — see
#     harness/guest/README.md for the fetch command
#   - the Node tarball (auto-fetched and hash-verified from VERSIONS)
set -euo pipefail
cd "$(dirname "$0")/.."

TIMEOUT_MINUTES=90

fail() { echo "FAIL: $1" >&2; exit 1; }

command -v qemu-system-riscv64 >/dev/null || fail "qemu-system-riscv64 missing (brew install qemu) — fail-closed per AGENTS.md"
command -v qemu-img >/dev/null || fail "qemu-img missing"

base=harness/guest/debian-sid-nocloud-riscv64.qcow2
[[ -f "$base" ]] || fail "missing $base — see harness/guest/README.md for the fetch command"

# Node tarball: pinned version + hash (first non-comment line of VERSIONS).
read -r node_ver node_tar node_sha < <(grep -v '^\s*#' harness/node/VERSIONS | grep -v '^\s*$' | head -1)
[[ -n "${node_ver:-}" && -n "${node_tar:-}" && -n "${node_sha:-}" ]] || fail "harness/node/VERSIONS has no entry"
node_path="harness/node/$node_tar"
if [[ ! -f "$node_path" ]]; then
  echo "== fetching $node_tar ==" >&2
  curl -fL --max-time 900 -o "$node_path" \
    "https://unofficial-builds.nodejs.org/download/release/$node_ver/$node_tar"
fi
echo "$node_sha  $node_path" | shasum -a 256 -c - >/dev/null || fail "Node tarball hash mismatch"

work="$(mktemp -d /tmp/vane-agent-image.XXXXXX)"
trap 'rm -rf "$work"' EXIT
results=harness/results
mkdir -p "$results"
log="$results/agent-image-build-$(date +%F-%H%M%S).log"

# Overlay so the base image is never mutated.
qemu-img create -f qcow2 -b "$base" -F qcow2 "$work/disk.qcow2" >/dev/null

# The in-guest provisioner. Installs Node, Pi, and the extension.
cat > "$work/provision.sh" <<'INNER'
#!/bin/bash
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
echo "=== AGENT IMAGE PROVISION START ==="

echo "== node =="
mkdir -p /opt
tar -xzf /opt/payload/node.tar.gz -C /opt
ln -sf /opt/node-v*-linux-riscv64/bin/node /usr/local/bin/node
ln -sf /opt/node-v*-linux-riscv64/bin/npm /usr/local/bin/npm
ln -sf /opt/node-v*-linux-riscv64/bin/npx /usr/local/bin/npx
node --version

echo "== ca-certificates + git (npm/pi install need them) =="
apt-get update -y
apt-get install -y --no-install-recommends ca-certificates git openssh-client
update-ca-certificates

echo "== pi =="
npm install -g @earendil-works/pi-coding-agent
pi --version

echo "== extension: github:gkgoat1/pi-surplus-intelligence =="
pi install github:gkgoat1/pi-surplus-intelligence

echo "== gate helpers =="
mkdir -p /opt/gates
cp /opt/payload/gates/* /opt/gates/ 2>/dev/null || true

echo "== smoke: node https via real DNS =="
node -e 'fetch("https://registry.npmjs.org/-/ping").then(r=>{console.log("HTTPS-OK",r.status)}).catch(e=>{console.error("HTTPS-FAIL",e.message);process.exit(1)})'

sync
echo "=== AGENT IMAGE PROVISION DONE ==="
poweroff -f
INNER

# Payload drive: raw tar on a virtio block device (no host fs tooling
# needed). Contents: node.tar.gz, provision.sh, gates/.
mkdir -p "$work/payload/gates"
cp "$node_path" "$work/payload/node.tar.gz"
cp "$work/provision.sh" "$work/payload/provision.sh"
cp harness/guest/gates/* "$work/payload/gates/" 2>/dev/null || true
tar -cf "$work/payload.tar" -C "$work/payload" node.tar.gz provision.sh gates

# NoCloud seed: extract the payload and run the provisioner, then power off.
cat > "$work/user-data" <<'UD'
#cloud-config
runcmd:
  - [ bash, -c, "mkdir -p /opt/payload && tar -xf /dev/vdb -C /opt/payload && bash /opt/payload/provision.sh" ]
UD
: > "$work/meta-data"
if command -v genisoimage >/dev/null; then
  genisoimage -output "$work/seed.iso" -volid cidata -joliet -rock "$work/user-data" "$work/meta-data" >/dev/null
elif command -v hdiutil >/dev/null; then
  mkdir -p "$work/iso"
  cp "$work/user-data" "$work/iso/user-data"
  cp "$work/meta-data" "$work/iso/meta-data"
  hdiutil makehybrid -iso -joliet -default-volume-name cidata -o "$work/seed.iso" "$work/iso" >/dev/null
else
  fail "need genisoimage or hdiutil for the seed ISO"
fi

echo "== booting QEMU (timeout ${TIMEOUT_MINUTES}m; log: $log) ==" >&2
set +e
timeout "${TIMEOUT_MINUTES}m" qemu-system-riscv64 \
  -machine virt -m 2048 -smp 2 \
  -nographic -serial mon:stdio \
  -drive file="$work/disk.qcow2",if=virtio,format=qcow2 \
  -drive file="$work/payload.tar",if=virtio,format=raw,readonly=on \
  -drive file="$work/seed.iso",if=virtio,format=raw,readonly=on,media=cdrom \
  -netdev user,id=n0 -device virtio-net-device,netdev=n0 \
  2>&1 | tee "$log"
rc=${PIPESTATUS[0]}
set -e
[[ $rc -eq 124 ]] && fail "QEMU timed out after ${TIMEOUT_MINUTES}m (log: $log)"

grep -q "AGENT IMAGE PROVISION DONE" "$log" || fail "provisioning did not complete (log: $log)"
grep -q "HTTPS-OK" "$log" || fail "guest HTTPS smoke test failed (log: $log)"

# Publish the built image as RAW (vane-cli's FileBlock backend is raw
# 512-byte-sector only; QCOW2 is a stretch goal). Not committed.
out=harness/agent-riscv64.img
qemu-img convert -f qcow2 -O raw "$work/disk.qcow2" "$out"
echo "PASS: agent image at $out (log: $log)" >&2
