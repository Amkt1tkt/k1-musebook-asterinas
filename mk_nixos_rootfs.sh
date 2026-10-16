#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: pack the complete NixOS closure into an ext2 rootfs image.
# Build configuration_k1_minimal.nix with the maintained nixpkgs pin, then run:
#   ./mk_nixos_rootfs.sh ./result /path/to/new/aster-rootfs.ext2
# Stage 1 resolves /nix/var/nix/profiles/system/init in this image.

set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: $0 <toplevel-store-path> [output.ext2]" >&2
    exit 2
fi
for tool in nix-store mkfs.ext2 e2fsck; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

TOPLEVEL=$(readlink -e -- "$1")
OUT_IMG=${2:-aster-rootfs.ext2}
if [[ "$TOPLEVEL" != /nix/store/* ]] || [ ! -x "$TOPLEVEL/init" ]; then
    echo "error: expected a NixOS store path with executable init" >&2
    exit 1
fi
if [ -e "$OUT_IMG" ] || [ -L "$OUT_IMG" ]; then
    echo "error: refusing to replace existing output: $OUT_IMG" >&2
    exit 1
fi
OUT_DIR=$(dirname -- "$OUT_IMG")
[ -d "$OUT_DIR" ] || { echo "error: output directory is missing: $OUT_DIR" >&2; exit 1; }

ROOTDIR=$(mktemp -d "${TMPDIR:-/tmp}/k1-rootfs.XXXXXX")
TMP_IMG=
cleanup() {
    rm -rf -- "$ROOTDIR"
    if [ -n "$TMP_IMG" ]; then rm -f -- "$TMP_IMG"; fi
}
trap cleanup EXIT
TMP_IMG=$(mktemp "$OUT_DIR/.k1-rootfs.XXXXXX")

echo "[mk-rootfs] assembling the closure at $ROOTDIR"
chmod 0755 "$ROOTDIR"
mkdir -p "$ROOTDIR"/nix/store "$ROOTDIR"/nix/var/nix/profiles
mkdir -p "$ROOTDIR"/{proc,sys,dev,tmp,run,etc,root,bin}
chmod 1777 "$ROOTDIR/tmp"
chmod 0700 "$ROOTDIR/root"
nix-store -qR "$TOPLEVEL" | while IFS= read -r path; do
    cp -a -- "$path" "$ROOTDIR/nix/store/"
done
ln -s -- "$TOPLEVEL" "$ROOTDIR/nix/var/nix/profiles/system"

# Nix closures have many small files; reserve inodes independently of bytes.
SIZE_MB=$(( $(du -sm "$ROOTDIR" | cut -f1) + 512 ))
NR_INODES=$(( $(find "$ROOTDIR" | wc -l) + 50000 ))
echo "[mk-rootfs] creating $OUT_IMG (${SIZE_MB} MiB, ${NR_INODES} inodes)"
truncate -s "${SIZE_MB}M" "$TMP_IMG"
mkfs.ext2 -q -F -b 4096 -L aster-rootfs -N "$NR_INODES" -d "$ROOTDIR" "$TMP_IMG"
e2fsck -f -n "$TMP_IMG"

# A same-directory hard link publishes the complete file atomically and fails
# if another process created the destination while the image was being built.
chmod 0644 "$TMP_IMG"
ln -T -- "$TMP_IMG" "$OUT_IMG"
echo "[mk-rootfs] ready: $OUT_IMG (GPT partition: rootfs; filesystem label: aster-rootfs)"
