#!/bin/sh

# SPDX-License-Identifier: MPL-2.0

# K1 MUSE Book: libdrm drmGetDevices2 sysfs acceptance check.

# Checks the sysfs entries that libdrm's `drmGetDevices2()` reads, doing the same
# operations on the same paths that `xf86drm.c` does.
#
# Checks selected platform-DRM sysfs contracts independently of libdrm.
# A pass does not prove all libdrm enumeration logic or rendering.
# Non-DRM character devices are outside this probe.
#
# The order below mirrors libdrm's own:
#
#   process_device()          stat /dev/dri/<node>, take its major:minor
#   drmNodeIsDRM()            stat /sys/dev/char/<major>:<minor>/device/drm
#   drmParseSubsystemType()   readlink .../device/subsystem, match the last
#                             path component against "platform"
#   drmParseOFBusInfo()       read OF_FULLNAME from .../device/uevent
#   drmParseOFDeviceInfo()    read OF_COMPATIBLE_N and OF_COMPATIBLE_<i>
#   drmGetMinorNameForFD()    opendir .../device/drm, look for card*/renderD*
#   drmGetDeviceNameFromFd2() read DEVNAME from .../uevent

set -e

# The roots are overridable so that the script can also be run against a mock
# tree on a development host, which is how its own logic gets checked.
SYS_DEV_CHAR=${SYS_DEV_CHAR:-/sys/dev/char}
DEV_DIR=${DEV_DIR:-/dev}
DRI_DIR="$DEV_DIR/dri"
# The DRM major number. Both nodes of every DRM device carry it.
DRM_MAJOR=226

failures=0
checks=0

fail() {
	echo "FAIL $1"
	failures=$((failures + 1))
}

ok() {
	checks=$((checks + 1))
	echo "ok   $1"
}

# Prints the value of a uevent key, or nothing if the key is absent.
uevent_get() {
	sed -n "s/^$2=//p" "$1" 2>/dev/null
}

check_entry() {
	entry="$1"
	name=$(basename "$entry")
	major=${name%%:*}
	minor=${name##*:}
	[ "$major" = "$DRM_MAJOR" ] || return 0
	case "$minor" in
		''|*[!0-9]*) fail "$name: invalid minor number"; return ;;
	esac

	# The entry's own uevent has to agree with the name of the entry, since the
	# name is the only thing tying a device node to this directory.
	if [ ! -f "$entry/uevent" ]; then
		fail "$entry/uevent does not exist"
		return
	fi
	if [ "$(uevent_get "$entry/uevent" MAJOR)" = "$major" ] &&
		[ "$(uevent_get "$entry/uevent" MINOR)" = "$minor" ]; then
		ok "$name: uevent reports MAJOR=$major MINOR=$minor"
	else
		fail "$name: uevent contradicts the directory name"
	fi

	# drmGetDeviceNameFromFd2 turns DEVNAME into a path under /dev.
	dev_name=$(uevent_get "$entry/uevent" DEVNAME)
	if [ -n "$dev_name" ]; then
		ok "$name: DEVNAME=$dev_name"
	else
		fail "$name: uevent has no DEVNAME"
	fi
	if [ -n "$dev_name" ] && [ ! -c "$DEV_DIR/$dev_name" ]; then
		fail "$name: $DEV_DIR/$dev_name is not a character device"
	elif [ -n "$dev_name" ]; then
		actual=$(stat -c '%t:%T' "$DEV_DIR/$dev_name")
		expected=$(printf '%x:%x' "$major" "$minor")
		if [ "$actual" = "$expected" ]; then
			ok "$name: character device number matches"
		else
			fail "$name: character device reports $actual, expected $expected"
		fi
	fi

	# drmParseSubsystemType readlinks this and matches the last component.
	if [ ! -L "$entry/device/subsystem" ]; then
		fail "$name: device/subsystem is not a symbolic link"
		return
	fi
	subsystem=$(readlink "$entry/device/subsystem")
	bus=$(basename "$subsystem")
	if [ "$bus" = platform ] || [ "$bus" = spi ]; then
		ok "$name: device/subsystem -> $subsystem (DRM_BUS_PLATFORM)"
	else
		fail "$name: device/subsystem -> $subsystem is not a platform bus"
	fi
	if [ ! -d "$entry/device/subsystem" ]; then
		fail "$name: device/subsystem does not resolve to a directory"
	fi

	# drmParseOFBusInfo and drmParseOFDeviceInfo read the parent's uevent.
	if [ ! -f "$entry/device/uevent" ]; then
		fail "$name: device/uevent does not exist"
		return
	fi
	fullname=$(uevent_get "$entry/device/uevent" OF_FULLNAME)
	modalias=$(uevent_get "$entry/device/uevent" MODALIAS)
	if [ -n "$fullname" ]; then
		ok "$name: OF_FULLNAME=$fullname"
	elif [ -n "$modalias" ]; then
		ok "$name: no OF_FULLNAME, but MODALIAS=$modalias is the documented fallback"
	else
		fail "$name: device/uevent has neither OF_FULLNAME nor MODALIAS"
	fi

	count=$(uevent_get "$entry/device/uevent" OF_COMPATIBLE_N)
	if [ -z "$count" ]; then
		# libdrm assumes a single entry in this case and then requires either
		# OF_COMPATIBLE_0 or MODALIAS.
		count=1
	fi
	case "$count" in
		''|*[!0-9]*) fail "$name: invalid OF_COMPATIBLE_N=$count"; return ;;
	esac
	# Bound this diagnostic, not the kernel ABI, against malformed input.
	if [ ${#count} -gt 3 ] || [ "$count" -gt 256 ]; then
		fail "$name: OF_COMPATIBLE_N exceeds this probe's 256-entry limit"
		return
	fi
	index=0
	while [ "$index" -lt "$count" ]; do
		compatible=$(uevent_get "$entry/device/uevent" "OF_COMPATIBLE_$index")
		if [ -n "$compatible" ]; then
			ok "$name: OF_COMPATIBLE_$index=$compatible"
		elif [ -n "$modalias" ]; then
			ok "$name: no OF_COMPATIBLE_$index, falling back to MODALIAS"
		else
			fail "$name: OF_COMPATIBLE_$index is missing and there is no MODALIAS"
		fi
		index=$((index + 1))
	done

	# The rest is DRM-specific, and only meaningful for a DRM device.
	if [ "$major" != "$DRM_MAJOR" ]; then
		return
	fi

	# drmNodeIsDRM: a node failing this is not treated as a DRM device at all.
	if [ -d "$entry/device/drm" ]; then
		ok "$name: device/drm exists (drmNodeIsDRM)"
	else
		fail "$name: device/drm does not exist, so libdrm ignores this node"
		return
	fi

	# drmGetMinorNameForFD scans that directory for a card*/renderD* entry.
	if [ -e "$entry/device/drm/$(basename "$dev_name")" ]; then
		ok "$name: device/drm contains $(basename "$dev_name")"
	else
		fail "$name: device/drm has no entry named after $dev_name"
	fi
}

echo "Start DRM sysfs test......"

if [ ! -d "$SYS_DEV_CHAR" ]; then
	echo "FAIL $SYS_DEV_CHAR does not exist"
	exit 1
fi
ok "$SYS_DEV_CHAR exists"

for entry in "$SYS_DEV_CHAR"/226:*; do
	[ -e "$entry" ] || continue
	check_entry "$entry"
done

# A /dev/dri character node without sysfs is a failure even in an empty tree.
if [ -d "$DRI_DIR" ]; then
	for node in "$DRI_DIR"/*; do
		[ -c "$node" ] || continue
		found=0
		for entry in "$SYS_DEV_CHAR"/226:*; do
			[ -f "$entry/uevent" ] || continue
			if [ "$(uevent_get "$entry/uevent" DEVNAME)" = "dri/$(basename "$node")" ]; then
				found=1
			fi
		done
		if [ "$found" -eq 0 ]; then
			fail "$node has no /sys/dev/char entry, so libdrm cannot see it"
		fi
	done
fi

if [ "$failures" -ne 0 ]; then
	echo "$failures of $((checks + failures)) DRM sysfs checks failed."
	exit 1
fi

if [ "$checks" -eq 1 ]; then
	echo "skip no DRM device/sysfs entry is present"
else
	echo "All $checks DRM sysfs checks passed."
fi
