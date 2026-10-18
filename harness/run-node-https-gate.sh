#!/usr/bin/env bash
# Node HTTPS gate (PLAN_PI_IN_VM P5/P6, stage 3): Node in the guest fetches
# an https:// URL through the tunnel with certificate validation.
#   --mitm   run through the host-side interception proxy instead (P6);
#            requires the generated CA installed into the guest trust store.
set -euo pipefail
cd "$(dirname "$0")/.."
source harness/lib-gate.sh

MITM=0
for a in "$@"; do
  case "$a" in
    --mitm) MITM=1 ;;
    *) gate_fail "unknown argument: $a" ;;
  esac
done

if [[ $MITM -eq 1 ]]; then
  gate_fail "--mitm mode not yet implemented (PLAN_PI_IN_VM §4.3: host MITM proxy + generated CA on the 9p share, NODE_EXTRA_CA_CERTS in-guest)"
fi

start=$(date +%s)
gate_boot_agent_image "03-https.sh"
gate_record "node-https" "$start" true "https fetch through tunnel, cert validation on"
