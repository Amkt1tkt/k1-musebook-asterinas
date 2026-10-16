#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: populate input properties before libinput enumerates evdev.
# This initializes database records but does not emit hotplug notifications.

set -euo pipefail

# Override roots for isolated tests; normal startup uses /sys and /run.
CLASS_DIR=${INPUT_UDEV_CLASS_DIR:-/sys/class/input}
DB_DIR=${INPUT_UDEV_DB_DIR:-/run/udev/data}

# Device dimensions do not substitute for an input type property.
TYPE_TAGS='
E:ID_INPUT_KEYBOARD=1
E:ID_INPUT_KEY=1
E:ID_INPUT_MOUSE=1
E:ID_INPUT_TOUCHPAD=1
E:ID_INPUT_TOUCHSCREEN=1
E:ID_INPUT_TABLET=1
E:ID_INPUT_TABLET_PAD=1
E:ID_INPUT_JOYSTICK=1
E:ID_INPUT_ACCELEROMETER=1
E:ID_INPUT_POINTINGSTICK=1
E:ID_INPUT_TRACKBALL=1
E:ID_INPUT_SWITCH=1
'

usage() {
  cat <<'EOF'
seed-input-udev-db [--check|--help]

Populate /run/udev/data/cMAJOR:MINOR for /sys/class/input/event*.
Device numbers come from each node's dev attribute.

  (no args)  classify devices and atomically replace their records
  --check    verify existing records and required input properties
  --help     show this help

Records satisfy udev metadata prerequisites; libinput still validates
device capabilities. Re-running updates records without notifying a
running libinput instance.

INPUT_UDEV_CLASS_DIR and INPUT_UDEV_DB_DIR override the directory roots
for isolated tests. Normal startup leaves them unset.
EOF
}

log() { printf 'seed-input-udev-db: %s\n' "$*"; }

# Read complete database lines, retaining the last value of each property.
# sd-device does not commit an unterminated last line.
db_has_input_tags() {
  local line='' key value has_input=0
  local -A type_values=()
  while IFS= read -r line; do
    case $line in
      E:ID_INPUT=*) has_input=0
        if [ "$line" = 'E:ID_INPUT=1' ]; then has_input=1; fi
        ;;
      E:ID_INPUT_*=*)
        key=${line%%=*}
        case $'\n'"$TYPE_TAGS" in
          *$'\n'"$key=1"$'\n'*) type_values["$key"]=${line#*=} ;;
        esac
        ;;
    esac
  done
  [ -z "$line" ] && [ "$has_input" = 1 ] || return 1
  for value in "${type_values[@]}"; do
    if [ "$value" = 1 ]; then return 0; fi
  done
  return 1
}

# Database names use the device number, not the event node basename.
read_devnum() {
  local syspath=$1 raw
  if [ ! -e "$syspath/dev" ]; then
    log "no $syspath/dev (kernel CharDevice should publish it)" >&2
    return 1
  fi
  raw=$(tr -d '[:space:]' < "$syspath/dev") || return 1
  if [[ $raw =~ ^[0-9]+:[0-9]+$ ]]; then
    printf '%s\n' "$raw"
  else
    log "$syspath/dev is '$raw', expected MAJOR:MINOR" >&2
    return 1
  fi
}

# input_id works without udevd. Test mode prints properties as KEY=value;
# convert them to the E: records consumed by sd-device.
try_udevadm() {
  local syspath=$1 out filtered line
  command -v udevadm >/dev/null 2>&1 || return 1
  out=$(udevadm test-builtin input_id "$syspath" 2>/dev/null) || return 1
  filtered=
  while IFS= read -r line || [ -n "$line" ]; do
    case $line in
      ID_INPUT*=*) filtered+="E:${line}"$'\n' ;;
    esac
  done <<< "$out"
  db_has_input_tags <<< "$filtered" || return 1
  printf '%s' "$filtered"
}

# Limited fallback for key, mouse, touchpad and switch devices when input_id fails.

# Linux input-event-codes.h constants.
EV_KEY=1
EV_REL=2
EV_ABS=3
EV_SW=5
REL_X=0
REL_Y=1
KEY_ESC=1
KEY_ENTER=28
BTN_MISC=256
BTN_MOUSE=272
BTN_TOUCH=330
BTN_TOOL_FINGER=325
ABS_X=0
ABS_Y=1

_bm=()

bm_load() {
  local file=$1
  _bm=()
  [ -f "$file" ] || return 1
  read -r -a _bm < "$file" || return 1
  [ "${#_bm[@]}" -gt 0 ]
}

# K1 capability bitmaps use 64-bit words, highest first, without leading
# zero words. Test a nibble at a time to avoid signed 64-bit shell arithmetic.
bm_has() {
  local bit=$1
  local n word_from_low word_idx word padded nib_idx bit_in_word bit_in_nib nib
  n=${#_bm[@]}
  [ "$n" -gt 0 ] || return 1
  word_from_low=$((bit / 64))
  if [ "$word_from_low" -ge "$n" ]; then
    return 1
  fi
  word_idx=$((n - 1 - word_from_low))
  word=${_bm[$word_idx]}
  word=${word#0x}
  # Short words omit leading zeroes; bit zero remains at the right edge.
  padded=$(printf '%016s' "$word" | tr ' ' '0')
  bit_in_word=$((bit % 64))
  nib_idx=$((15 - bit_in_word / 4))
  bit_in_nib=$((bit_in_word % 4))
  nib=${padded:nib_idx:1}
  case $nib in
    [0-9a-fA-F]) nib=$(printf '%d' "0x$nib") ;;
    *) return 1 ;;
  esac
  [ $(( (nib >> bit_in_nib) & 1 )) -eq 1 ]
}

bm_has_any_key() {
  local i=$KEY_ESC
  while [ "$i" -lt "$BTN_MISC" ]; do
    if bm_has "$i"; then
      return 0
    fi
    i=$((i + 1))
  done
  return 1
}

try_sysfs() {
  local syspath=$1
  local caps evf keyf relf absf
  local has_ev_key=0 has_ev_rel=0 has_ev_abs=0 has_ev_sw=0
  local tags=''

  caps="$syspath/device/capabilities"
  evf="$caps/ev"
  keyf="$caps/key"
  relf="$caps/rel"
  absf="$caps/abs"

  if ! bm_load "$evf"; then
    return 1
  fi
  if bm_has "$EV_KEY"; then has_ev_key=1; fi
  if bm_has "$EV_REL"; then has_ev_rel=1; fi
  if bm_has "$EV_ABS"; then has_ev_abs=1; fi
  if bm_has "$EV_SW"; then has_ev_sw=1; fi

  # Relative pointer, including the touchpad in k1.touchpad=rel mode.
  if [ "$has_ev_rel" = 1 ] && bm_load "$relf" && bm_has "$REL_X" && bm_has "$REL_Y"; then
    if bm_load "$keyf" && bm_has "$BTN_MOUSE"; then
      tags+='E:ID_INPUT_MOUSE=1'$'\n'
    fi
  fi

  # K1 absolute touchpads advertise contact and finger-tool capabilities.
  # This fallback does not distinguish other absolute-device properties.
  if [ "$has_ev_abs" = 1 ] && bm_load "$absf" && bm_has "$ABS_X" && bm_has "$ABS_Y"; then
    if bm_load "$keyf" && bm_has "$BTN_TOUCH" && bm_has "$BTN_TOOL_FINGER"; then
      tags+='E:ID_INPUT_TOUCHPAD=1'$'\n'
    fi
  fi

  # Lid and other switch devices need a type property as well.
  if [ "$has_ev_sw" = 1 ]; then
    tags+='E:ID_INPUT_SWITCH=1'$'\n'
  fi

  if [ "$has_ev_key" = 1 ] && bm_load "$keyf" && bm_has_any_key; then
    tags+='E:ID_INPUT_KEY=1'$'\n'
    if bm_has "$KEY_ENTER"; then
      tags+='E:ID_INPUT_KEYBOARD=1'$'\n'
    fi
  fi

  [ -n "$tags" ] || return 1
  printf 'E:ID_INPUT=1\n%s' "$tags"
}

write_db() {
  local id=$1 content=$2 dest tmp
  dest="$DB_DIR/c${id}"
  mkdir -p -- "$DB_DIR" || return 1
  tmp=$(mktemp -- "$DB_DIR/.tmp.XXXXXX") || return 1
  # Set permissions before atomic publication; sd-device needs a final newline.
  if printf '%s\n' "${content%$'\n'}" > "$tmp" &&
      chmod 0644 -- "$tmp" &&
      mv -fT -- "$tmp" "$dest"; then
    return 0
  fi
  rm -f -- "$tmp" || true
  return 1
}

check_one() {
  local syspath=$1 id dest
  id=$(read_devnum "$syspath") || return 1
  dest="$DB_DIR/c${id}"
  if [ ! -f "$dest" ]; then
    log "missing $dest (from $syspath)" >&2
    return 1
  fi
  if ! db_has_input_tags < "$dest"; then
    log "invalid $dest: require newline-terminated input properties" >&2
    sed 's/^/  /' "$dest" >&2 || true
    return 1
  fi
  log "ok $syspath -> $dest"
  sed 's/^/  /' "$dest"
}

seed_one() {
  local syspath=$1 id content method
  id=$(read_devnum "$syspath") || return 1
  if content=$(try_udevadm "$syspath"); then
    method=udevadm
  elif content=$(try_sysfs "$syspath"); then
    method=sysfs
  else
    log "could not classify $syspath (udevadm missing/failed, and capabilities did not yield a type tag)" >&2
    return 1
  fi
  if ! write_db "$id" "$content"; then
    log "could not write $DB_DIR/c${id}" >&2
    return 1
  fi
  log "$syspath -> $DB_DIR/c${id} ($method)"
  # Command substitution strips the final newline; restore it for logging.
  printf '%s\n' "$content" | sed 's/^/  /'
}

list_event_nodes() {
  local d
  shopt -s nullglob
  for d in "$CLASS_DIR"/event*; do
    printf '%s\n' "$d"
  done
}

main() {
  local mode=seed check_only=0
  if [ "$#" -gt 1 ]; then
    usage >&2
    return 2
  fi
  case ${1:-} in
    --help|-h) usage; return 0 ;;
    --check) mode=check; check_only=1 ;;
    '') ;;
    *)
      log "unknown argument: $1" >&2
      usage >&2
      return 2
      ;;
  esac

  if [ ! -d "$CLASS_DIR" ]; then
    log "$CLASS_DIR is missing — kernel input class not published; nothing to seed"
    return 0
  fi

  local nodes n=0 failed=0 node
  nodes=$(list_event_nodes) || return 1
  if [ -z "$nodes" ]; then
    log "no $CLASS_DIR/event* yet (USB probe late? re-run this script)"
    return 0
  fi

  while IFS= read -r node; do
    n=$((n + 1))
    if [ "$check_only" = 1 ]; then
      check_one "$node" || failed=$((failed + 1))
    else
      seed_one "$node" || failed=$((failed + 1))
    fi
  done <<< "$nodes"

  if [ "$failed" -ne 0 ]; then
    log "$mode: $failed of $n device(s) failed"
    return 1
  fi
  log "$mode: $n device(s) ok"
  return 0
}

if [[ ${BASH_SOURCE[0]} == "$0" ]]; then
  main "$@"
fi
