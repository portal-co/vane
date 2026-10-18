# /opt/gates — in-guest gate helpers

Scripts here are baked into the agent image by `build-agent-image.sh` and
invoked by the host-side gates over the console. One script per chain stage
(PLAN_PI_IN_VM §7): `02-node.sh`, `03-https.sh`, `04-npm.sh`, `05-pi.sh`.
