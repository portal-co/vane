#!/usr/bin/env bash
# M4 device-model gate (PLAN_LINUX_WASM.md §7.4): the machine payload must
# print "M1:TTTK" and power off cleanly under vane-cli --machine, natively
# and under WASI.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building payload-machine ==" >&2
(cd harness/payload-machine && cargo build --release)

payload="harness/payload-machine/target/riscv64gc-unknown-none-elf/release/payload-machine"

echo "== native ==" >&2
out="$(cargo run -q -p vane-cli --no-default-features -- \
  --elf "$payload" --machine --hart --uart-rx V 2>/dev/null)"
echo "output: $out" >&2
[[ "$out" == "M1:TTTK"* ]] || { echo "FAIL: unexpected output: $out" >&2; exit 1; }

echo "== wasm32-wasip1 under node:wasi ==" >&2
cargo build -q -p vane-cli --no-default-features --target wasm32-wasip1 --release
out="$(node harness/run-wasi.mjs target/wasm32-wasip1/release/vane-cli.wasm \
  --elf "$payload" --machine --hart --uart-rx V 2>/dev/null)"
echo "output: $out" >&2
[[ "$out" == "M1:TTTK"* ]] || { echo "FAIL: unexpected output under WASI: $out" >&2; exit 1; }

echo "M4 machine gate: PASS" >&2
