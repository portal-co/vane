#!/usr/bin/env bash
# Fetch the Debian sid *generic* riscv64 kernel (linux-binary-*.deb) for the
# disk-rootfs gates (PLAN_PI_IN_VM R8).
#
# Why not the netboot installer kernel: the netboot Image is a DIFFERENT
# version from sid's current generic kernel and its initrd carries only
# installer-minimal modules (no virtio_blk/ext4/jbd2/9p). The generic
# linux-binary + linux-modules packages are the consistent pair, and
# fetch-virtio-modules.sh reads the version magic off whatever kernel image
# you point it at.
#
# Output: harness/linux/linux-generic (+ the .deb, gitignored).
set -euo pipefail
cd "$(dirname "$0")"

echo "== locating the sid riscv64 generic kernel ==" >&2
pkgfile="$(curl -fsL --max-time 300 "http://deb.debian.org/debian/dists/sid/main/binary-riscv64/Packages.gz" | gzip -dc)"
filename="$(awk '/^Package: linux-binary-[0-9].*-riscv64$/{f=1} f&&/^Filename:/{print $2; exit}' <<<"$pkgfile")"
[[ -n "$filename" ]] || { echo "no linux-binary-*-riscv64 package in the sid index" >&2; exit 1; }

deb="${filename##*/}"
if [[ ! -f "$deb" ]]; then
  echo "== fetching $deb ==" >&2
  curl -fL --max-time 900 -o "$deb" "http://deb.debian.org/debian/$filename"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
ar p "$deb" data.tar.xz > "$tmp/data.tar.xz"
vmlinuz="$(tar -tJf "$tmp/data.tar.xz" | grep 'vmlinuz$' | head -1)"
tar -xJf "$tmp/data.tar.xz" -C "$tmp" "$vmlinuz"
cp "$tmp/$vmlinuz" linux-generic
echo "linux-generic: $(strings -a linux-generic | grep -m1 -o 'Linux version [^ ]*' || true)" >&2
echo "now run: ./fetch-virtio-modules.sh ./linux-generic" >&2
