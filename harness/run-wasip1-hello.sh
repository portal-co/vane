#!/usr/bin/env bash
# M0 gate (PLAN_LINUX_WASM.md §10): build the RV64 payload and the WASI
# preview1 build of vane-cli, run the payload inside the WASI module under
# a WASI host (Node's node:wasi), and assert the expected console output.
#
# Requirements: rustup targets `wasm32-wasip1` and
# `riscv64gc-unknown-none-elf`, and Node >= 20.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building RV64 payload ==" >&2
# NOTE: build from inside the payload directory so cargo picks up its
# .cargo/config.toml (target + linker script); cargo's config discovery is
# cwd-based, not manifest-based.
(cd harness/payload-hello && cargo build --release)

echo "== building vane-cli (wasm32-wasip1, no JS host) ==" >&2
cargo build -p vane-cli --no-default-features --target wasm32-wasip1 --release

# The wasip1 build must not pull in any JS-engine bindings.
if cargo tree -p vane-cli --no-default-features --target wasm32-wasip1 \
    | grep -qiE 'js-sys|wasm-bindgen|web-sys'; then
  echo "FAIL: wasip1 build tree contains JS bindings" >&2
  exit 1
fi

echo "== running payload inside the WASI module ==" >&2
out="$(node harness/run-wasi.mjs target/wasm32-wasip1/release/vane-cli.wasm \
  --elf harness/payload-hello/target/riscv64gc-unknown-none-elf/release/payload-hello 2>/dev/null)"
echo "$out"

if [[ "$out" != *"hello from vane wasip1"* ]]; then
  echo "FAIL: expected payload output missing" >&2
  exit 1
fi
echo "M0 wasip1 gate: PASS" >&2
