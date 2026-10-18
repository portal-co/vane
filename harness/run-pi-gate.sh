#!/usr/bin/env bash
# Pi gate (PLAN_PI_IN_VM P8, stage 5): Pi runs non-interactively in the
# guest with github:gkgoat1/pi-surplus-intelligence installed, against the
# mock provider (harness/mock-provider) replaying a recorded session, so the
# gate is deterministic and offline.
#
# Host side starts: mock-provider replay on 127.0.0.1:18181, mapped into the
# guest as a static --net-map entry. The guest's Pi is pointed at the mock
# via env (no real credentials — §11 Sec-1). With PI_GATE_REAL=1 and a real
# provider key in the environment, runs against the real provider instead.
set -euo pipefail
cd "$(dirname "$0")/.."
source harness/lib-gate.sh

start=$(date +%s)

if [[ "${PI_GATE_REAL:-0}" != "1" ]]; then
  gate_need harness/mock-provider/recordings/pi-session.jsonl \
    "record one first: python3 harness/mock-provider/server.py --record <upstream> --recording pi-session (during a real run)"
  python3 harness/mock-provider/server.py --port 18181 --recording pi-session &
  mock_pid=$!
  trap 'kill "$mock_pid" 2>/dev/null || true' EXIT
  sleep 1
fi

gate_boot_agent_image "05-pi.sh"
gate_record "pi" "$start" true "pi -p against mock provider, extension loaded"
