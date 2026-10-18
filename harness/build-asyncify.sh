#!/usr/bin/env bash
# Build a vane wasm module with Binaryen asyncify instrumentation, so async
# host imports (non-local host round-trips) can suspend the guest.
#
# Usage: harness/build-asyncify.sh <crate> <import-glob>...
#   <crate>        workspace crate with a cdylib wasm target (e.g. vane-web)
#   <import-glob>  asyncify import patterns to treat as suspending, e.g.
#                  'env.vane_*' — these are the non-local host round-trips.
#
# Produces <crate>/pkg-asyncify/ with the asyncify-instrumented module.
set -euo pipefail
cd "$(dirname "$0")/.."

crate="${1:?usage: build-asyncify.sh <crate> <import-glob>...}"
shift
imports=("$@")

echo "== building $crate for wasm32-unknown-unknown ==" >&2
(cd "crates/$crate" && wasm-pack build --target web --offline --out-dir pkg-asyncify)

wasm="$(ls "crates/$crate"/pkg-asyncify/*_bg.wasm | head -1)"
echo "== applying wasm-opt --asyncify to $wasm ==" >&2
args=(--asyncify -O2)
for g in "${imports[@]}"; do
  args+=(--pass-arg "asyncify-imports@$g")
done
# shellcheck disable=SC2068
wasm-opt ${args[@]} "$wasm" -o "$wasm"

echo "asyncify module ready at crates/$crate/pkg-asyncify/" >&2
