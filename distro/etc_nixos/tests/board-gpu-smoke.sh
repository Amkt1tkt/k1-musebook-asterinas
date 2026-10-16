#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: explicit board GPU smoke; no modeset or benchmark is requested.
# Run on the board serial console. Host fixtures only check script control flow.
# Firmware/node checks are mandatory. An uninstalled userspace tier is SKIP;
# an installed but broken tier is FAIL. No result proves rendering correctness.
set -uo pipefail

command -v timeout >/dev/null 2>&1 || { echo 'ERROR: coreutils timeout is required' >&2; exit 2; }
umask 077
work=$(mktemp -d "${TMPDIR:-/tmp}/k1-gpu-smoke.XXXXXXXX") || exit 2
trap 'rm -rf -- "$work"' EXIT
pass=0 fail=0 skip=0
failed_names=()

check() {
    local name=$1
    shift
    printf '%-46s ' "$name"
    if timeout --kill-after=2 15 "$@" >"$work/check.out" 2>&1; then
        echo PASS
        pass=$((pass + 1))
    else
        local rc=$?
        echo "FAIL (rc=$rc)"
        head -12 "$work/check.out" | sed 's/^/    /'
        fail=$((fail + 1))
        failed_names+=("$name")
    fi
}
note() { printf '%-46s SKIP  %s\n' "$1" "$2"; skip=$((skip + 1)); }
have() { command -v "$1" >/dev/null 2>&1; }

echo '== 1. installed firmware =='
check 'firmware image installed' test -f /lib/firmware/powervr/rogue_36.29.52.182_v1.fw
check 'firmware licence installed' test -f /lib/firmware/powervr/LICENSE.powervr
check 'firmware image is 126976 bytes' sh -c '[ "$(stat -c %s /lib/firmware/powervr/rogue_36.29.52.182_v1.fw)" = 126976 ]'

echo '== 2. DRM nodes and enumeration =='
check 'card0 present' test -c /dev/dri/card0
check 'renderD128 present' test -c /dev/dri/renderD128
if have modetest; then
    check 'modetest enumerates the primary node' modetest -c
    head -1 "$work/check.out" | sed 's/^/  /'
else
    note 'modetest enumerates the primary node' 'libdrm tools not installed'
fi

echo '== 3. dynamic program startup =='
if have vulkaninfo; then
    check 'vulkaninfo reaches its help handler' vulkaninfo --help
elif have modetest; then
    # modetest -h uses a nonzero usage status on some versions. The successful
    # -c above already exercises its dynamic loader without masking any status.
    note 'dynamic program startup' 'covered by modetest -c above'
else
    note 'dynamic program startup' 'no selected dynamic tool installed'
fi

echo '== 4. Vulkan =='
if have vulkaninfo; then
    # The shipped profile exports an ICD directory. Loader file/list overrides
    # are also valid; do not turn a missing manifest into an uninstalled tier.
    check 'configured PowerVR ICD manifest exists' bash -c '
        IFS=: read -r -a paths <<< "${VK_DRIVER_FILES:-${VK_ICD_FILENAMES:-}}"
        for p in "${paths[@]}"; do
            if [[ -d $p ]]; then
                for f in "$p"/powervr_mesa_icd.*.json; do [[ -f $f ]] && exit 0; done
            elif [[ -f $p ]]; then
                grep -qi powervr "$p" && exit 0
            fi
        done
        exit 1'
    check 'non-conformant PVR opt-in set' sh -c '[ -n "${PVR_I_WANT_A_BROKEN_VULKAN_DRIVER:-}" ]'
    check 'vulkaninfo --summary succeeds' vulkaninfo --summary
    cp "$work/check.out" "$work/vulkan.out"
    check 'summary names BXE-2-32' grep -q BXE-2-32 "$work/vulkan.out"
else
    note 'Vulkan enumeration' 'vulkaninfo not installed'
fi

echo '== 5. EGL/GBM over Zink =='
if have eglinfo; then
    check 'MESA_LOADER_DRIVER_OVERRIDE unset' sh -c '[ -z "${MESA_LOADER_DRIVER_OVERRIDE:-}" ]'
    # mesa-demos 9.0.0 accepts -p gbm; it has no -d card0 argument.
    # Restricting the platform avoids a surfaceless/Wayland fallback passing.
    check 'eglinfo on GBM succeeds' eglinfo -B -p gbm
    cp "$work/check.out" "$work/egl.out"
    check 'GBM initialized an EGL display' grep -q '^EGL API version:' "$work/egl.out"
    check 'GBM reports an OpenGL ES renderer' grep -Eq 'OpenGL ES.*renderer string:' "$work/egl.out"
    check 'GBM renderer names zink' grep -Ei 'renderer string:.*zink' "$work/egl.out"
    echo '  GBM default-device evidence only; this does not identify the exact card0/renderD128 pair.'
else
    note 'EGL/GBM over Zink' 'eglinfo not installed'
fi

printf 'pass=%s fail=%s skip=%s\n' "$pass" "$fail" "$skip"
if (( fail )); then
    printf 'Failed: %s\n' "${failed_names[@]}"
    echo 'Check the failed items above and confirm the running kernel/rootfs versions.'
    echo 'For ICD failures: VK_LOADER_DEBUG=all vulkaninfo --summary (can be verbose).'
fi
(( fail == 0 ))
