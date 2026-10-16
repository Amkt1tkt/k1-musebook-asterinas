#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
#
# K1 MUSE Book: confirm BXE-2-32 BVNC is in the Mesa source / ICD.
# Native Linux drm-shim acceptance fixture for BVNC 36.29.52.182 (BXE-2-32).
# It checks ICD discovery and Mesa device initialization against a fabricated
# GPU. It does not test the RISC-V build, real firmware, rendering, or DMA/KMS.
# Runs only when explicitly invoked: it clones and builds in the chosen workdir.
# Hardware validation remains necessary even after this script passes.
#
# Requires: git, meson, ninja, pkg-config, python3-mako, LLVM dev, libdrm dev,
#           Vulkan loader + `vulkaninfo` (vulkan-tools).
#
# Usage:
#   ./mesa-bvnc-acceptance.sh [workdir]

set -euo pipefail

# Match the pinned rootfs Mesa release unless explicitly overridden.
# Report and validate the actual checkout; an existing workdir is not evidence
# that the requested tag was cloned.
MESA_TAG="${MESA_TAG:-mesa-26.1.6}"
BVNC="${BVNC:-36.29.52.182}"

workdir="${1:-$PWD/mesa-bvnc-acceptance}"
mkdir -p "$workdir"
cd "$workdir"

if [ ! -d mesa ]; then
    echo "== cloning Mesa $MESA_TAG (shallow) =="
    git clone --depth 1 --branch "$MESA_TAG" \
        https://gitlab.freedesktop.org/mesa/mesa.git mesa
fi

cd mesa
[[ $(git rev-parse --show-toplevel) == "$(pwd -P)" ]] || {
    echo 'FAIL: mesa is not the root of its own Git checkout' >&2; exit 1;
}
expected=$(git rev-parse --verify "refs/tags/$MESA_TAG^{commit}") || {
    echo "FAIL: local checkout does not contain tag $MESA_TAG" >&2; exit 1;
}
actual=$(git rev-parse HEAD)
[[ $actual == "$expected" ]] || {
    echo "FAIL: checkout HEAD $actual differs from $MESA_TAG ($expected); left unchanged" >&2; exit 1;
}
[[ -z $(git status --porcelain --untracked-files=normal) ]] || {
    echo 'FAIL: source checkout is not clean; left unchanged' >&2; exit 1;
}
[[ $BVNC == 36.29.52.182 ]] || {
    echo 'FAIL: this acceptance script only verifies BXE-2-32 / 36.29.52.182' >&2; exit 1;
}
command -v timeout >/dev/null 2>&1 || { echo 'FAIL: coreutils timeout is required' >&2; exit 2; }
echo "Mesa source: tag=$MESA_TAG commit=$actual"

echo ""
echo "== checking this tag actually knows the BVNC =="
# Without this the run below would still "succeed" -- against a different GPU.
info=src/imagination/common/device_info/bxe-2-32.h
if [ ! -f "$info" ]; then
    echo "FAIL: $info absent from $MESA_TAG; it needs to be >= mesa-25.3.0" >&2
    exit 1
fi
grep -q 'public_name = "BXE-2-32"' "$info" ||
    { echo "FAIL: $info does not name BXE-2-32" >&2; exit 1; }
echo "ok: $info present"

echo ""
echo "== configuring =="
# `-Dtools=drm-shim` is what builds libpowervr_noop_drm_shim.so. llvmpipe is
# here only because Mesa wants at least one gallium driver.
setup=()
[[ ! -f build/meson-private/coredata.dat ]] || setup+=(--reconfigure)
meson setup "${setup[@]}" build \
    -Dbuildtype=release \
    -Dvulkan-drivers=imagination \
    -Dgallium-drivers=llvmpipe \
    -Dtools=drm-shim \
    -Dplatforms=

echo ""
echo "== building =="
ninja -C build

shim=$(find build -name 'libpowervr_noop_drm_shim.so' -print -quit)
icd=$(find build -name 'powervr_mesa_icd.*.json' -print -quit)
[ -n "$shim" ] || { echo "FAIL: drm-shim not built" >&2; exit 1; }
[ -n "$icd" ] || { echo "FAIL: ICD manifest not generated" >&2; exit 1; }

echo ""
echo "== running vulkaninfo against a shimmed $BVNC =="
echo "shim: $shim"
echo "icd:  $icd"
echo ""

# PVR_I_WANT_A_BROKEN_VULKAN_DRIVER is mandatory, not belt-and-braces:
# pvr_device_is_conformant() returns true only for BXS-4-64 (36.53.104.796), so
# on this BVNC physical-device creation fails with
# VK_ERROR_INCOMPATIBLE_DRIVER without it. That is also why the run below is a
# real test of the *conformance gate* and not just of the device-info table.
out=$(
    timeout --kill-after=2 30 env LD_PRELOAD="$PWD/$shim" \
        VK_DRIVER_FILES="$PWD/$icd" \
        VK_ICD_FILENAMES="$PWD/$icd" \
        PVR_SHIM_DEVICE_BVNC="$BVNC" \
        PVR_I_WANT_A_BROKEN_VULKAN_DRIVER=1 \
        vulkaninfo --summary 2>&1
) && rc=0 || rc=$?

echo "$out"
echo ""

if [ "$rc" -ne 0 ]; then
    echo "FAIL: vulkaninfo exited $rc"
    echo ""
    echo "Read the output above before concluding Mesa lacks support:"
    echo "  * 'not a conformant Vulkan implementation' -> the env var did not"
    echo "    reach the driver."
    echo "  * 'failed to stat DRM render node' -> LD_PRELOAD did not take;"
    echo "    vulkaninfo may be a wrapper script, so preload onto the real ELF."
    echo "  * 'unsupported BVNC' / device-info lookup failure -> this is the"
    echo "    real negative result. It means Mesa does not know this GPU after"
    echo "    all, and the acceleration route needs reassessing."
    exit 1
fi

if ! grep -q 'BXE-2-32' <<< "$out"; then
    echo "FAIL: vulkaninfo ran but did not report BXE-2-32."
    echo "It probably enumerated a different ICD, or drm-shim fell back to its"
    echo "default BVNC of 36.53.104.796 (BXS-4-64) because PVR_SHIM_DEVICE_BVNC"
    echo "was not parsed."
    exit 1
fi

echo "PASS: native Mesa $MESA_TAG ($actual) initializes a shim device for BVNC $BVNC (BXE-2-32); hardware untested."
