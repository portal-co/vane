# Agent guest image (PLAN_PI_IN_VM §5, M7)

The agent disk image is a glibc Debian riscv64 disk (Node + Pi +
pre-installed extensions) built by `../build-agent-image.sh`. Artifacts here
are fetched/built, not committed.

## Fetch the base image (once, ~400 MiB)

```sh
latest=$(curl -s https://cdimage.debian.org/images/cloud/sid/daily/ \
  | grep -oE '20[0-9]+-[0-9]+/' | sort | tail -1)
curl -fL -o harness/guest/debian-sid-nocloud-riscv64.qcow2 \
  "https://cdimage.debian.org/images/cloud/sid/daily/${latest}debian-sid-nocloud-riscv64-daily-${latest%/}.qcow2"
```

Then: `harness/build-agent-image.sh` (needs Homebrew QEMU; fail-closed).

## Environment contract for agents running in the VM

| What | Where |
| --- | --- |
| Node.js / npm / npx | `/usr/local/bin` (tarball under `/opt/node-v*-linux-riscv64`) |
| Pi | global npm install; `pi` on PATH |
| Extensions | pre-installed via `pi install`; live development sources under `/mnt/host` |
| Gate helpers | `/opt/gates/` |
| Host files (9p) | `/mnt/host` (repo checkout, fixtures, CA bundle when `--mitm`) |
| Guest-owned state | the virtio-blk root disk (rootfs, npm cache, `~/.pi`) |
| Networking | guest `10.0.2.15/24`, gateway `10.0.2.2`, DNS `10.0.2.3`; all TCP tunneled to host sockets (WASIX/`std::net`); DNS resolved host-side |
| Clock | no RTC device; the host appends `vane_epoch=<unix>` to the kernel cmdline and the guest preamble (`/opt/gates/00-preamble.sh`, or the init script) runs `date -s @<epoch>` — TLS cert validation depends on this |

Caveats: first runs after a cold boot are slow (interpreter); gates restore
snapshots (S0–S3) instead. Credentials are never in the image — they arrive
via the environment at gate time.
