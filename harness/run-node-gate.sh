#!/usr/bin/env bash
# Node gate (PLAN_PI_IN_VM P4, stage 2): Node runs in the guest.
# Requires the agent image (harness/build-agent-image.sh).
#
# Guest gate script: /opt/gates/02-node.sh runs `node --version`,
# `node -e` hello, a crypto check, and a compute microbenchmark whose
# result feeds the §9 performance table.
set -euo pipefail
cd "$(dirname "$0")/.."
source harness/lib-gate.sh

start=$(date +%s)
gate_boot_agent_image "02-node.sh"
# (unreachable until gate_boot_agent_image is implemented)
gate_record "node" "$start" true "node $NODE_VERSION in guest"
