# Linux boot artifacts for the M4 gate

These are fetched once and not committed (large binaries):

- `opensbi.bin` — copied from QEMU's bundled OpenSBI:
  `cp "$(brew --prefix)/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin" opensbi.bin`
- `linux` — Debian riscv64 netboot kernel Image:
  `curl -LO https://deb.debian.org/debian/dists/sid/main/installer-riscv64/current/images/netboot/debian-installer/riscv64/linux`
- `alpine-minirootfs-20230329-riscv64.tar.gz` — Alpine minirootfs (busybox):
  `curl -LO https://dl-cdn.alpinelinux.org/alpine/edge/releases/riscv64/alpine-minirootfs-20230329-riscv64.tar.gz`
- `initrd.gz` — built from the minirootfs by `../build-initramfs.sh`
  (adds our `/init` test script and packs a gzip'd newc cpio).

Boot:

```sh
cargo run -p vane-cli --no-default-features -- --machine \
  --opensbi opensbi.bin --kernel linux --initrd initrd.gz \
  --append "console=ttyS0 panic=-1"
```

The interpreter is slow (~1M instr/s), so expect the boot to take tens of
minutes. `VANE_HEARTBEAT=1` prints a per-round pc/priv/mcause/mip heartbeat
to stderr.
