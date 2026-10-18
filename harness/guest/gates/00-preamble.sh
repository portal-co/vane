# shellcheck shell=bash
# Gate preamble (sourced by every /opt/gates script): set the guest clock
# from the host-provided epoch (PLAN_PI_IN_VM R7). vane has no RTC device;
# the host passes `vane_epoch=<unix>` on the kernel cmdline and TLS cert
# validation depends on this being set.
_epoch=$(sed -n 's/.*vane_epoch=\([0-9]*\).*/\1/p' /proc/cmdline)
if [ -n "$_epoch" ]; then
  date -s "@$_epoch" >/dev/null 2>&1 || date -s "$_epoch" >/dev/null 2>&1 || true
fi
echo "gate-clock: $(date -u 2>/dev/null || echo unknown)"
