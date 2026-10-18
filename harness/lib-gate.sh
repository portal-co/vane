#!/usr/bin/env bash
# Shared helpers for the agent-chain gates (PLAN_PI_IN_VM §7).
# Sourced by run-*-gate.sh scripts; not executable on its own.

gate_fail() { echo "FAIL: $1" >&2; exit 1; }

# Require a file, with a pointer to where it comes from (fail-closed).
gate_need() { # path, hint
  [[ -f "$1" ]] || gate_fail "missing $1 — $2"
}

# Kernel-cmdline fragment setting the guest clock (PLAN_PI_IN_VM R7):
# gates append this to --append; the guest preamble (00-preamble.sh) reads
# it back from /proc/cmdline and runs date -s. vane has no RTC device.
gate_epoch_arg() {
  echo "vane_epoch=$(date +%s)"
}

# Record gate timing to harness/results/<date>-<stage>.json.
gate_record() { # stage, started_at_epoch, pass(true|false), notes
  local stage="$1" start="$2" pass="$3" notes="$4"
  local end; end=$(date +%s)
  mkdir -p harness/results
  cat > "harness/results/$(date +%F)-${stage}.json" <<JSON
{"stage":"$stage","started_at":$start,"wall_seconds":$((end-start)),"from_snapshot":${FROM_SNAPSHOT:-null},"pass":$pass,"host_os":"$(uname -s)","runtime":"vane-cli native","notes":"$notes"}
JSON
}

# RAM sizing note (learned from R8 boots): the 128MiB default OOMs even the
# minimal initramfs flow; 512MiB works for the disk gate. The agent-image
# gates (Node/npm/Pi) should start at 1-2 GiB (--ram-size 0x40000000+),
# paired with --swap if the host working set matters.
#
# Boot the agent disk image under vane-cli with the tunnel attached, running
# /opt/gates/<script> as init. Requires the built image and a kernel that can
# mount the virtio-blk rootfs.
#
# NOT YET RUNNABLE: needs (a) the built agent image (build-agent-image.sh),
# (b) the boot flow for a disk-rootfs guest (kernel with virtio-blk built in
# + an init that mounts / and execs the gate script — the S-snapshot chain
# exists to make this fast). See PLAN_PI_IN_VM §6-§7.
gate_boot_agent_image() { # gate_script_name
  local gate_script="$1"
  gate_need harness/agent-riscv64.img "run harness/build-agent-image.sh first"
  gate_need harness/linux/linux "see harness/linux/README.md"
  gate_need harness/linux/opensbi.bin "see harness/linux/README.md"
  gate_fail "disk-rootfs boot flow not yet implemented (PLAN_PI_IN_VM §7; expected: boot kernel with 'root=/dev/vda rw console=ttyS0 init=/opt/gates/${gate_script}' and --disk harness/agent-riscv64.img --net)"
}
