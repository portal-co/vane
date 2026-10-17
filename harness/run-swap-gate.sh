#!/usr/bin/env bash
# M5 swap gate (PLAN_LINUX_WASM.md §6, exit criteria S4 + S5): the swap-stress
# payload cycles more guest RAM than the resident-set cap through the
# encrypted backing store and verifies integrity with its own checksum, under
# both native and WASI hosts.
#
#   payload writes/verifies a per-page xorshift64* pattern across a 6 MiB
#   region with a 512 KiB resident cap → ~88 pages evict to the encrypted
#   swap and fault back; the payload reports S4:<checksum>:OK.
#
# S5 (tamper detection) is covered by crates/vane-swap/tests/file_swap.rs
# (run in the workspace test suite) plus the unit tests in the crate.
set -euo pipefail
cd "$(dirname "$0")/.."

RESIDENT=0x80000   # 512 KiB resident = 8 frames
SWAP_FILE="target/swap-gate.bin"
EXPECTED_MARK="S4:342551bdf2c401d6:OK"

echo "== building payload-swap-stress ==" >&2
(cd harness/payload-swap-stress && cargo build --release)

payload="harness/payload-swap-stress/target/riscv64gc-unknown-none-elf/release/payload-swap-stress"
mkdir -p target
rm -f "$SWAP_FILE"

echo "== native (resident $RESIDENT) ==" >&2
out="$(cargo run -q -p vane-cli --no-default-features -- \
  --elf "$payload" --machine --engine trace \
  --swap "$SWAP_FILE" --resident "$RESIDENT" 2>/dev/null)"
echo "output: $out" >&2
[[ "$out" == *"$EXPECTED_MARK"* ]] || { echo "FAIL: expected $EXPECTED_MARK, got: $out" >&2; exit 1; }

# The swap file must exist and hold ciphertext (non-trivial size).
[[ -s "$SWAP_FILE" ]] || { echo "FAIL: swap file not created" >&2; exit 1; }

echo "== wasm32-wasip1 under node:wasi ==" >&2
rm -f "$SWAP_FILE"
cargo build -q -p vane-cli --no-default-features --target wasm32-wasip1 --release
out="$(node harness/run-wasi.mjs target/wasm32-wasip1/release/vane-cli.wasm \
  --elf "$payload" --machine --engine trace \
  --swap "$SWAP_FILE" --resident "$RESIDENT" 2>/dev/null)"
echo "output: $out" >&2
[[ "$out" == *"$EXPECTED_MARK"* ]] || { echo "FAIL: expected $EXPECTED_MARK under WASI, got: $out" >&2; exit 1; }

echo "M5 swap gate: PASS" >&2
