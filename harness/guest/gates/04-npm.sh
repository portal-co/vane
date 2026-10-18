#!/bin/bash
# Stage 4 gate (P7): cold-cache npm install of a pinned tiny package.
set -uo pipefail
echo "=== GATE NPM ==="
rm -rf /tmp/npm-gate && mkdir -p /tmp/npm-gate && cd /tmp/npm-gate
npm init -y >/dev/null
npm --version || { echo "GATE-FAIL: npm --version"; poweroff -f; }
rm -rf ~/.npm  # cold cache
npm install --no-audit --no-fund left-pad@1.3.0 || { echo "GATE-FAIL: npm install"; poweroff -f; }
node -e 'const lp=require("/tmp/npm-gate/node_modules/left-pad");if(lp("7",3,"0")!=="007")throw new Error("bad");console.log("npm-ok")' \
  || { echo "GATE-FAIL: require"; poweroff -f; }
echo "=== GATE NPM OK ==="
poweroff -f
