#!/usr/bin/env bash
# Stage the browser demo: build the wasm package and copy it plus the boot
# artifacts next to index.html. Run from anywhere; paths are resolved.
set -euo pipefail
cd "$(dirname "$0")/../.."

here="demo/browser"

echo "== building wasm package ==" >&2
(cd crates/vane-web && wasm-pack build --target web --offline)

echo "== copying pkg ==" >&2
rm -rf "$here/pkg"
cp -r crates/vane-web/pkg "$here/pkg"

echo "== copying boot artifacts ==" >&2
for f in opensbi.bin linux initrd.gz; do
  if [ -f "harness/linux/$f" ]; then
    cp "harness/linux/$f" "$here/$f"
  else
    echo "WARN: harness/linux/$f missing (see harness/linux/README.md)" >&2
  fi
done

echo "staged. Serve with: (cd $here && python3 -m http.server 8000)" >&2
