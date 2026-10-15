#!/usr/bin/env bash
# M1 gate (PLAN_LINUX_WASM.md §4, S1 + D5):
#   1. Run the RV64 payload inside Node with `new Function`/`eval`/
#      `WebAssembly.compile*` disabled — both the trace interpreter and the
#      JIT-with-fallback paths must still produce the correct output.
#   2. Run again with codegen allowed, and require engine parity everywhere.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building RV64 payload ==" >&2
(cd harness/payload-hello && cargo build --release)

echo "== building wasm-pack pkg (nodejs target) ==" >&2
(cd crates/vane && wasm-pack build --target nodejs --out-dir pkg)

payload="harness/payload-hello/target/riscv64gc-unknown-none-elf/release/payload-hello"

echo "== no-JIT environment ==" >&2
node harness/nojit-gate.mjs "$payload"

echo "== codegen-allowed reference ==" >&2
node harness/nojit-gate.mjs --allow-codegen "$payload"

echo "M1 no-JIT gate: PASS" >&2
