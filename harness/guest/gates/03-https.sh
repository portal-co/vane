#!/bin/bash
# Stage 3 gate (P5/P6): HTTPS fetch through the tunnel with cert validation.
# With --mitm the host exports NODE_EXTRA_CA_CERTS=/mnt/host/mitm/ca.pem via
# the gate driver.
set -uo pipefail
source /opt/gates/00-preamble.sh

echo "=== GATE HTTPS ==="
node -e '
fetch("https://registry.npmjs.org/-/ping")
  .then(r => { if (!r.ok) throw new Error("status "+r.status); console.log("https-ok", r.status); })
  .catch(e => { console.error("GATE-FAIL:", e.message); process.exit(1); })
' || { echo "GATE-FAIL: https fetch"; poweroff -f; }
echo "=== GATE HTTPS OK ==="
poweroff -f
