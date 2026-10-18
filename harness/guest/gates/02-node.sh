#!/bin/bash
# Stage 2 gate (P4): Node runs in the guest.
set -uo pipefail
echo "=== GATE NODE ==="
node --version || { echo "GATE-FAIL: node --version"; poweroff -f; }
node -e 'console.log("node-hello-ok")' || { echo "GATE-FAIL: node -e"; poweroff -f; }
node -e 'const c=require("crypto");console.log("crypto-ok",c.createHash("sha256").update("vane").digest("hex").slice(0,8))' \
  || { echo "GATE-FAIL: crypto"; poweroff -f; }
# Microbenchmark for the PLAN §9 table: BigInt spin.
node -e 'const t=Date.now();let x=1n;for(let i=0n;i<2_000_000n;i++)x=(x*6364136223846793005n+1442695040888963407n)&0xffffffffffffffffn;console.log("bench-ms",Date.now()-t,x&1n)'
echo "=== GATE NODE OK ==="
poweroff -f
