#!/usr/bin/env bash
# M4 Linux boot gate (PLAN_LINUX_WASM.md §7.5, milestones B1–B4):
# boots OpenSBI + Debian riscv64 kernel + our initramfs under vane-cli
# --machine, and requires:
#   - the OpenSBI banner ("Platform Name : vane-virt")
#   - the kernel reaching userspace ("Run /init as init process")
#   - real programs running correctly in userspace:
#       uname -a prints the kernel release
#       sha256sum of a known string matches the reference digest
#   - clean poweroff via SBI SRST ("reboot: Power down")
#
# NOTE: the interpreter runs ~1M instr/s, so a full boot takes on the order
# of 30 minutes. This is a manual/nightly gate, not a per-commit CI job.
set -euo pipefail
cd "$(dirname "$0")/.."

artifacts=harness/linux
for f in opensbi.bin linux initrd.gz; do
  [[ -f "$artifacts/$f" ]] || {
    echo "missing $artifacts/$f — see harness/linux/README.md" >&2
    exit 2
  }
done

echo "== building vane-cli ==" >&2
cargo build -q -p vane-cli --no-default-features

echo "== booting Linux (this takes a while) ==" >&2
out="$(cargo run -q -p vane-cli --no-default-features -- \
  --machine --opensbi "$artifacts/opensbi.bin" --kernel "$artifacts/linux" \
  --initrd "$artifacts/initrd.gz" --append "console=ttyS0 panic=-1" 2>/dev/null)"

fail() { echo "FAIL: $1" >&2; exit 1; }
grep -q "Platform Name               : vane-virt" <<<"$out" || fail "no OpenSBI banner"
grep -q "Linux version 7\." <<<"$out" || fail "no kernel version banner"
grep -q "Run /init as init process" <<<"$out" || fail "never reached /init"
grep -q "riscv64 Linux" <<<"$out" || fail "uname did not print riscv64 Linux"
grep -q "23a3ca670126b0a4a7d441a73b81eda47d8bfa9f6da80d30dcdba1943e6db009" <<<"$out" \
  || fail "sha256sum mismatch (wrong digest for 'hello-from-busybox')"
grep -q "reboot: Power down" <<<"$out" || fail "no clean poweroff"

echo "M4 Linux boot gate: PASS" >&2
