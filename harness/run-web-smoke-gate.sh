#!/usr/bin/env bash
# M6 web smoke gate: the browser wasm surface (vane-web) runs a swap-stress
# guest end-to-end in Node with a JS-provided swap store — the same shape the
# OPFS bridge uses in a dedicated worker. This is the offline-verifiable core
# of the browser path; the full Playwright browser E2E (S3) is network-gated
# (needs npm + browser download) and lives in demo/browser/e2e.spec.mjs.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== building payload-swap-stress ==" >&2
(cd harness/payload-swap-stress && cargo build -q --release)

echo "== building vane-web (nodejs target) ==" >&2
(cd crates/vane-web && wasm-pack build --target nodejs --offline --out-dir pkg-node)

echo "== running Node smoke (wasm + swap bridge) ==" >&2
node demo/browser/node-smoke.mjs

echo "M6 web smoke gate: PASS" >&2
