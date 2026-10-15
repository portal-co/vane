#!/usr/bin/env bash
# M3 gate (PLAN_LINUX_WASM.md §7): the privileged-architecture payload must
# pass under vane-cli both natively and inside WASI (no JS anywhere).
#
# The payload boots in M-mode, builds Sv39 page tables, drops to S-mode,
# exercises ecall delegation (M then S), translated loads/stores, and a
# delegated store page fault, then reports via the riscv-tests `tohost`
# convention (0 = pass).
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building payload-priv ==" >&2
(cd harness/payload-priv && cargo build --release)

payload="harness/payload-priv/target/riscv64gc-unknown-none-elf/release/payload-priv"

echo "== native (plain-Rust twin) ==" >&2
cargo run -q -p vane-cli --no-default-features -- \
  --elf "$payload" --hart --tohost 0x80010000
echo "native: PASS" >&2

echo "== wasm32-wasip1 under node:wasi ==" >&2
cargo build -q -p vane-cli --no-default-features --target wasm32-wasip1 --release
node harness/run-wasi.mjs target/wasm32-wasip1/release/vane-cli.wasm \
  --elf "$payload" --hart --tohost 0x80010000
echo "wasip1: PASS" >&2

echo "M3 privileged gate: PASS" >&2
