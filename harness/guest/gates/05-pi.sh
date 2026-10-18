#!/bin/bash
# Stage 5 gate (P8): Pi non-interactive with the extension, against the
# mock provider (base URL + dummy key exported by the gate driver; never
# real credentials — PLAN §11 Sec-1).
set -uo pipefail
source /opt/gates/00-preamble.sh

echo "=== GATE PI ==="
pi --version || { echo "GATE-FAIL: pi --version"; poweroff -f; }
pi extensions list 2>&1 | grep -q "pi-surplus-intelligence" \
  || { echo "GATE-FAIL: extension not installed"; poweroff -f; }
echo "GATE-INFO: extension pi-surplus-intelligence present"
pi -p "Reply with exactly: GATE-PI-OK" --print 2>&1 | tee /tmp/pi-out
grep -q "GATE-PI-OK" /tmp/pi-out || { echo "GATE-FAIL: pi non-interactive run"; poweroff -f; }
echo "=== GATE PI OK ==="
poweroff -f
