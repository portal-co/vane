#!/usr/bin/env bash
# npm gate (PLAN_PI_IN_VM P7, stage 4): cold-cache `npm install` of a pinned
# tiny package in the guest, then `node -e require()` of it.
set -euo pipefail
cd "$(dirname "$0")/.."
source harness/lib-gate.sh

start=$(date +%s)
gate_boot_agent_image "04-npm.sh"
gate_record "npm" "$start" true "cold-cache npm install of pinned package"
