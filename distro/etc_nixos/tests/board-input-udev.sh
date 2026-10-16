#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: check input enumeration and udev records on the board.
# needs-hardware: run on the K1 serial console with the board configuration installed.
#
# Enumeration needs both a working udev monitor and initialized records.
# An explicit-device libinput run bypasses enumeration, but still uses udev
# metadata. Use a direct evdev read/ioctl test to check the kernel interface.
# Monitor diagnostics use debug-events --verbose; list-devices has no verbose
# mode, and libudev does not honor SYSTEMD_LOG_LEVEL.

set -uo pipefail
command -v timeout >/dev/null 2>&1 || { echo 'ERROR: coreutils timeout is required' >&2; exit 2; }
umask 077
work=$(mktemp -d "${TMPDIR:-/tmp}/k1-input-smoke.XXXXXXXX") || exit 2
trap 'rm -rf -- "$work"' EXIT

pass=0
fail=0
skip=0
failed_names=()

check() {
    local name=$1
    shift
    printf '%-52s ' "$name"
    if timeout --kill-after=2 10 "$@" > "$work/check.out" 2>&1; then
        echo PASS
        pass=$((pass + 1))
    else
        echo FAIL
        head -12 "$work/check.out" | sed 's/^/    /'
        fail=$((fail + 1))
        failed_names+=("$name")
    fi
}

note() {
    printf '%-52s SKIP  %s\n' "$1" "$2"
    skip=$((skip + 1))
}

have() { command -v "$1" > /dev/null 2>&1; }

echo "== 1. kernel input class (the files this script reads) =="
check "/sys/class/input exists" test -d /sys/class/input

shopt -s nullglob
event_nodes=(/sys/class/input/event*)
if [ "${#event_nodes[@]}" -eq 0 ]; then
    check "event* nodes present" false
else
    check "at least one event* node" test "${#event_nodes[@]}" -ge 1
    echo "  (nodes: ${event_nodes[*]})"
fi

echo ""
echo "== 2. /run/udev/data/cMAJOR:MINOR, one file per event node =="
if have seed-input-udev-db; then
    check "seed-input-udev-db --check" seed-input-udev-db --check
else
    note "seed-input-udev-db --check" "script not on PATH (input.udev-db off?)"
fi

for syspath in "${event_nodes[@]}"; do
    name=$(basename "$syspath")
    if [ ! -e "$syspath/dev" ]; then
        check "$name has a dev attribute" false
        continue
    fi
    # $1 is expanded by the child shell.
    # shellcheck disable=SC2016
    if ! id=$(timeout --kill-after=2 3 bash -c 'tr -d "[:space:]" < "$1"' _ "$syspath/dev") ||
        [[ ! $id =~ ^[0-9]+:[0-9]+$ ]]; then
        check "$name has a readable major:minor attribute" false
        continue
    fi
    dest=/run/udev/data/c$id
    check "$name -> $dest exists" test -f "$dest"
    check "$dest has E:ID_INPUT=1" grep -qx 'E:ID_INPUT=1' "$dest"
    check "$dest has a type tag" \
        grep -Eq '^E:ID_INPUT_(KEYBOARD|KEY|MOUSE|TOUCHPAD|TOUCHSCREEN|TABLET|TABLET_PAD|JOYSTICK|ACCELEROMETER|POINTINGSTICK|TRACKBALL|SWITCH)=1$' "$dest"
    echo "  ($dest)"
    timeout --kill-after=2 3 head -40 "$dest" | sed 's/^/    /'
done

echo ""
echo "== 3. libinput: verbose udev monitor, then list-devices, then path =="
# Do not pass --verbose to list-devices; see the header. debug-events
# --verbose is the udev backend and is the only libinput tool that
# prints "failed to create / set up filter / bind the udev monitor".
# If assign_seat fails it exits at once. If it succeeds it waits for
# events. Timeout alone is inconclusive: initialization could also be stuck.
# A DEVICE_ADDED record proves enumeration reached an actual input device.
# Keep the run bounded and distinguish that evidence from an idle timeout.
if have libinput; then
    if have timeout; then
        printf '%-52s ' "libinput debug-events --verbose (monitor log)"
        # Capture the timeout status without set -e aborting this script.
        # status without aborting; require DEVICE_ADDED before reporting PASS.
        ec=0
        timeout --kill-after=2 3 libinput debug-events --verbose \
            > "$work/debug-events.out" 2>&1 || ec=$?
        if { [ "$ec" -eq 0 ] || [ "$ec" -eq 124 ]; } &&
            grep -Eq '^[[:space:]]*[-+]?event[0-9]+[[:space:]]+DEVICE_ADDED' "$work/debug-events.out"; then
            echo PASS
            pass=$((pass + 1))
        elif [ "$ec" -eq 0 ] || [ "$ec" -eq 124 ]; then
            echo 'SKIP (inconclusive: no DEVICE_ADDED record)'
            skip=$((skip + 1))
        else
            echo FAIL
            fail=$((fail + 1))
            failed_names+=("libinput debug-events --verbose (monitor log)")
        fi
        echo "  (libinput debug-events --verbose; timeout 3s, exit $ec; 124 alone does not prove monitor initialization)"
        head -40 "$work/debug-events.out" | sed 's/^/    /'
    else
        note "libinput debug-events --verbose (monitor log)" \
            "no timeout(1); do not run debug-events without a deadline (it waits for events)"
    fi

    if [ "${#event_nodes[@]}" -eq 0 ]; then
        note "libinput list-devices names a node" "no event nodes"
    else
        check "libinput list-devices names a /dev/input/event* node" \
            bash -o pipefail -c 'libinput list-devices | grep "/dev/input/event"'
        head -12 "$work/check.out" | sed 's/^/    /'
    fi
    if [ -c /dev/input/event0 ]; then
        # Help proves program startup, not path-backend or event delivery.
        check "libinput help handler runs" libinput debug-events --help
    else
        note "libinput help handler runs" "no /dev/input/event0"
    fi
else
    note "libinput debug-events --verbose (monitor log)" "libinput not installed"
    note "libinput list-devices names a node" "libinput not installed"
    note "libinput help handler runs" "libinput not installed"
fi

echo ""
echo "pass=$pass fail=$fail skip=$skip"

if [ "$fail" -ne 0 ]; then
    echo ""
    echo "Next checks for each failed input condition:"
    for name in "${failed_names[@]}"; do
        case $name in
        "/sys/class/input exists"|"event* nodes present"|*"has a dev attribute")
            echo "  Kernel input class, not this module. ls /sys/class/input"
            echo "    and /sys/devices/virtual/input/"
            ;;
        *"exists"|*"E:ID_INPUT"*|*"type tag"|"seed-input-udev-db --check")
            echo "  seed-input-udev-db          # write the files"
            echo "  command -v udevadm && udevadm test-builtin input_id /sys/class/input/event0"
            echo "    stdout should include ID_INPUT=1 and a type tag."
            ;;
        "libinput debug-events --verbose (monitor log)"|"libinput list-devices names a /dev/input/event* node")
            echo "  libinput debug-events --verbose"
            echo "    'failed to create the udev monitor' is socket / SO_PASSCRED / bind."
            echo "    'failed to bind the udev monitor' is SO_ATTACH_FILTER."
            echo "    Do not use list-devices --verbose (usage error) or"
            echo "    SYSTEMD_LOG_LEVEL=debug (libudev ignores it)."
            echo "  libinput debug-events --device /dev/input/event0"
            echo "    this bypasses enumeration but still requires udev metadata."
            echo "    Check device records, seat assignment and monitor diagnostics;"
            echo "    use a direct evdev read/ioctl test to isolate the kernel interface."
            ;;
        esac
    done
fi

[ "$fail" -eq 0 ]
