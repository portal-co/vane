#!/usr/bin/env bash
# Fetch extra kernel modules (9p's netfs/fscache, virtio-net's
# net_failover/failover) from the Debian pool, matched to the exact kernel
# release the initramfs's existing modules were built for (read from their
# vermagic). The t-26ef flow: find linux-modules-<ver> in the sid riscv64
# Packages index, download, extract the .ko files we need.
#
# Output: harness/linux/modules-extra/*.ko (picked up by
# build-initramfs-e2e.sh). Idempotent: skips the download when present.
set -euo pipefail
cd "$(dirname "$0")/linux"

# fscache is deliberately absent: Debian 7.2.11 builds CONFIG_FSCACHE
# built-in (the __fscache_* symbols 9p imports come from vmlinuz itself).
need=(netfs failover net_failover)

# ── The exact kernel release from the existing modules' vermagic ────────────
vermagic=$(strings virtio_blk.ko 2>/dev/null | grep -m1 '^vermagic=' || true)
if [[ -z "$vermagic" && -f "./lib/modules/virtio_blk.ko" ]]; then
  vermagic=$(strings ./lib/modules/virtio_blk.ko 2>/dev/null | grep -m1 '^vermagic=' || true)
fi
if [[ -z "$vermagic" ]]; then
  # The modules live inside initrd-virtio.gz — pull virtio_blk.ko out.
  work0="$(mktemp -d)"
  gzip -dc initrd-virtio.gz > "$work0/base.cpio"
  (cd "$work0" && cpio -idm --quiet base.cpio 2>/dev/null || cpio -idm < base.cpio >/dev/null 2>&1)
  # cpio needs the entry name: try both layouts.
  ko=""
  for cand in virtio_blk.ko lib/modules/virtio_blk.ko ./lib/modules/virtio_blk.ko; do
    [[ -f "$work0/$cand" ]] && ko="$work0/$cand" && break
  done
  if [[ -z "$ko" ]]; then
    (cd "$work0" && gzip -dc "${OLDPWD:-.}/initrd-virtio.gz" | cpio -idm --quiet >/dev/null 2>&1)
    ko=$(find "$work0" -name virtio_blk.ko | head -1)
  fi
  [[ -n "$ko" ]] && vermagic=$(strings "$ko" 2>/dev/null | grep -m1 '^vermagic=' || true)
  rm -rf "$work0"
fi
[[ -n "$vermagic" ]] || { echo "could not read vermagic (no virtio_blk.ko anywhere)" >&2; exit 1; }
rel=${vermagic#vermagic=}; rel=${rel%% *}
echo "kernel release: $rel" >&2

# ── Idempotence ──────────────────────────────────────────────────────────────
have_all=1
for m in "${need[@]}"; do
  [[ -f "modules-extra/$m.ko" ]] || have_all=0
done
if (( have_all )); then
  echo "modules-extra already complete" >&2
  exit 0
fi
mkdir -p modules-extra

# ── The sid riscv64 package index ────────────────────────────────────────────
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
idx="$work/Packages.xz"
url="http://deb.debian.org/debian/dists/sid/main/binary-riscv64/Packages.xz"
echo "fetching $url" >&2
curl -fsSL --retry 3 -o "$idx" "$url" || {
  echo "curl failed (network/LuLu gate?) — see AGENTS.md" >&2
  exit 1
}
xz -dc "$idx" > "$work/Packages"

# ── The modules package for this release ─────────────────────────────────────
pkg="linux-modules-$rel"
enc=${pkg//+/%2b}
line=$(grep -A1 "^Package: $pkg\$" "$work/Packages" | grep '^Filename:' | head -1 || true)
if [[ -n "$line" ]]; then
  deb_path=${line#Filename: }
  deb_url="http://deb.debian.org/debian/$deb_path"
else
  # sid keeps only the newest kernel; older ones live in the snapshot
  # archive. The .deb filename is predictable from the binary version
  # (<srcver> = the kernel image's own Debian source version, e.g.
  # 7.2.11-1 from "Debian 7.2.11-1" in linux-generic's strings).
  # Anchor on the release we know (7.2.11+deb14-riscv64 -> 7.2.11): a bare
  # 'Debian X-Y' grep also matches the toolchain banner (gcc's 16.2.0-3).
  base=${rel%%+*}
  # (the substitution needs `|| true`: grep -m1 closes the pipe after one
  # match, the 36MB `strings` stream then eats SIGPIPE, and pipefail turns
  # that into exit 141 — the same pipefail+grep -m1 bug t-26ef hit on the
  # first fetch script; the match itself is already printed by then)
  srcver=$(strings linux-generic 2>/dev/null \
    | grep -om1 "Debian $base-[0-9][0-9]*" | awk '{print $2}' || true)
  [[ -n "$srcver" ]] || {
    echo "cannot read the Debian source version from linux-generic" >&2
    exit 1
  }
  ts=${SNAPSHOT_TS:-20261025T000000Z}
  deb_url="https://snapshot.debian.org/archive/debian/$ts/pool/main/l/linux/${pkg}_${srcver}_riscv64.deb"
fi
echo "downloading $deb_url" >&2
curl -fsSL --retry 3 -o "$work/mods.deb" "$deb_url"

# ── Extract the .ko files we need (bsdtar reads .deb ar archives) ────────────
mkdir -p "$work/x"
bsdtar -xf "$work/mods.deb" -C "$work/x" data.tar.xz data.tar.zst data.tar 2>/dev/null || true
for d in "$work/x"/data.tar.*; do
  [[ -e "$d" ]] || continue
  bsdtar -xf "$d" -C "$work/x" 2>/dev/null || true
done
# Debian ships modules compressed (.ko.xz) under /usr/lib/modules/.
resolve_ko() {
  local m="$1" hit
  hit=$(find "$work/x" -name "$m.ko" | head -1 || true)
  if [[ -z "$hit" ]]; then
    hit=$(find "$work/x" -name "$m.ko.xz" | head -1 || true)
    if [[ -n "$hit" ]]; then
      xz -dc "$hit" > "$work/x/$m.ko"
      hit="$work/x/$m.ko"
    fi
  fi
  printf '%s' "$hit"
}

found=$(resolve_ko netfs)
[[ -n "$found" ]] || { echo "netfs.ko(.xz) not in the deb — wrong package?" >&2; exit 1; }

# Verify the vermagic matches before installing anything.
vm=$(strings "$found" | grep -m1 '^vermagic=' || true)
[[ "$vm" == "$vermagic" ]] || {
  echo "vermagic mismatch: fetched '$vm' != ours '$vermagic'" >&2
  exit 1
}
for m in "${need[@]}"; do
  ko="$(resolve_ko "$m")"
  [[ -n "$ko" ]] || { echo "MISSING: $m.ko not in the package" >&2; exit 1; }
  cp "$ko" "modules-extra/$m.ko"
  echo "  modules-extra/$m.ko" >&2
done
echo "modules-extra complete (vermagic $rel)" >&2
