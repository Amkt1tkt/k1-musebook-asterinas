# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: COSMIC session with a private D-Bus session bus.
# Asterinas has no systemd user manager. Use the system bus when available,
# otherwise run the battery and brightness bridges on the session bus.

set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "k1-cosmic-session: must be root; the builtin seat backend opens devices directly" >&2
  exit 1
fi

# Inherited display variables select a nested compositor instead of KMS.
for stale in WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY; do
  if [[ -v $stale ]]; then
    echo "k1-cosmic-session: unsetting inherited $stale=${!stale} (selects a nested backend)"
    unset "$stale"
  fi
done

export XDG_RUNTIME_DIR=/run/user/0
export XDG_SESSION_TYPE=wayland

# Recreate volatile runtime state for manual starts as well as boot activation.
# Session-bus sockets require a private directory owned by the session user.
ensure_dir() {
  local path=$1 mode=$2
  mkdir -p "$path" && chmod "$mode" "$path" \
    || echo "k1-cosmic-session: failed to create $path mode $mode (non-fatal)" >&2
}
ensure_dir "$XDG_RUNTIME_DIR" 0700

system_bus_socket=

pick_system_bus_socket() {
  local p
  for p in /run/dbus/system_bus_socket /var/run/dbus/system_bus_socket; do
    if [ -S "$p" ]; then
      system_bus_socket=$p
      return 0
    fi
  done
  return 1
}

# Wait only for a bus that systemd is already starting. Configurations without
# a system bus use the private bus immediately.
system_bus_ready() {
  if [ "${COSMIC_FORCE_DBUS_ALIAS:-}" = 1 ]; then
    echo "k1-cosmic-session: COSMIC_FORCE_DBUS_ALIAS=1; using the session-bus alias"
    return 1
  fi
  if pick_system_bus_socket; then
    return 0
  fi
  local state
  state=$(systemctl is-active dbus.service 2>/dev/null || true)
  case "$state" in
    active|activating)
      local i=0
      while [ "$i" -lt 3 ]; do
        if pick_system_bus_socket; then
          return 0
    fi
        sleep 1
        i=$((i + 1))
      done
      echo "k1-cosmic-session: dbus.service is $state but its socket is missing; using the session-bus alias" >&2
      return 1
      ;;
  esac
  return 1
}

# Optional device bridges must not delay or prevent desktop startup.
kick_system_bus_daemons() {
  if ! command -v systemctl >/dev/null 2>&1; then
    return 0
  fi
  systemctl start --no-block upower-shim.service 2>/dev/null || true
  systemctl start --no-block logind-shim.service 2>/dev/null || true
}

export XDG_CURRENT_DESKTOP=COSMIC
export HOME=/root
export LIBSEAT_BACKEND=builtin
export RUST_BACKTRACE=full
# Child INFO logs can saturate the UART and block the desktop.
: "${RUST_LOG:=warn}"
export RUST_LOG

# The Nix wrapper supplies component desktop files and complete schema
# overlays. Include the system profile even when /etc/profile was not sourced.
# shellcheck disable=SC2154 # Assigned by cosmic.nix before embedding this script.
_data_dirs="$cosmic_component_data_dirs"
if [ -n "${XDG_DATA_DIRS:-}" ]; then
  _data_dirs="$_data_dirs:$XDG_DATA_DIRS"
fi
case ":$_data_dirs:" in
  *:/run/current-system/sw/share:*) ;;
  *) _data_dirs="$_data_dirs:/run/current-system/sw/share" ;;
esac
export XDG_DATA_DIRS="$_data_dirs"

use_system_bus=0
if system_bus_ready; then
  use_system_bus=1
  export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$system_bus_socket"
  echo "k1-cosmic-session: system bus at $system_bus_socket"
else
  unset DBUS_SYSTEM_BUS_ADDRESS || true
  echo "k1-cosmic-session: no system bus socket; using the session-bus alias"
fi

# libinput needs an initialized udev database to accept evdev devices.
# The input module is optional; a seeding failure must not stop the session.
if [ -x /run/current-system/sw/bin/seed-input-udev-db ]; then
  /run/current-system/sw/bin/seed-input-udev-db \
    || echo "k1-cosmic-session: seed-input-udev-db failed; libinput will skip uninitialized devices" >&2
fi

# COSMIC's PulseAudio clients poll non-blockingly until their NOFAIL connection
# reaches Ready. A null sink prevents that busy loop without audio hardware.
# Start it inside dbus-run-session: clients already waiting for a server need
# org.pulseaudio.Server to appear on their own session bus.
#
# Skip hardware probing, shared-memory transports and rtkit. This root-only
# session uses a local anonymous socket. Keep daemon output in volatile storage
# and keep its child ownership in the private-bus shell for session cleanup.
# shellcheck disable=SC2317
start_pulse_stub() {
  if [ "${COSMIC_NO_PULSE_STUB:-}" = 1 ]; then
    echo "k1-cosmic-session: COSMIC_NO_PULSE_STUB=1; PulseAudio clients may busy-wait"
    return 0
  fi
  if ! command -v pulseaudio >/dev/null 2>&1; then
    echo "k1-cosmic-session: pulseaudio is missing; PulseAudio clients may busy-wait" >&2
    return 0
  fi
  pulseaudio --daemonize=no --disallow-exit --exit-idle-time=-1 --use-pid-file=no \
      --disable-shm=yes --realtime=no --high-priority=no -n \
      -L 'module-null-sink sink_name=dummy sink_properties=device.description=Dummy' \
      -L 'module-native-protocol-unix auth-anonymous=1' \
      --log-target=stderr --log-level=notice >/run/pulseaudio.stderr 2>&1 &
}
export -f start_pulse_stub

# The private-bus shell owns the session and its helpers. Clean up only its
# live background jobs; completed children must not leave stale PID records.
# shellcheck disable=SC2317
session_body() {
  local use_system_bus=$1
  shift
  local session_pid status=0
  stop_session_children() {
    local child
    while IFS= read -r child; do
      kill "$child" 2>/dev/null || true
      # A stopped child must resume to handle its pending termination signal.
      kill -CONT "$child" 2>/dev/null || true
    done < <(jobs -pr; jobs -ps)
  }
  trap stop_session_children EXIT
  trap 'exit 129' HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM
  if [ "$use_system_bus" = 0 ]; then
    export DBUS_SYSTEM_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS"
    if command -v upower-shim >/dev/null 2>&1; then
      upower-shim >/run/upower-shim.stderr 2>&1 &
    fi
    if command -v logind-shim >/dev/null 2>&1; then
      logind-shim >/run/logind-shim.stderr 2>&1 &
    fi
  fi
  start_pulse_stub
  cosmic-session "$@" &
  session_pid=$!
  wait "$session_pid" || status=$?
  stop_session_children
  trap - EXIT HUP INT TERM
  return "$status"
}
export -f session_body

sess_log=/run/cosmic-session.log
echo "k1-cosmic-session: starting COSMIC; session log: $sess_log"
# Retain only the current session's panel output.
: > /run/cosmic-panel.stderr 2>/dev/null || true

run_session() {
  if [ "$use_system_bus" = 1 ]; then
    kick_system_bus_daemons
  fi
  # The packaged child Bash retains exported functions and owns the helpers.
  # shellcheck disable=SC2016
  dbus-run-session -- bash -c 'session_body "$@"' cosmic-session "$use_system_bus" "$@"
}

# Preserve the session's exit status through tee. Failure to create a log
# must not prevent startup; console output remains available.
status=0
if : >"$sess_log"; then
  run_session "$@" 2>&1 | tee "$sess_log" || status=${PIPESTATUS[0]}
else
  echo "k1-cosmic-session: cannot create $sess_log; using console output only" >&2
  run_session "$@" || status=$?
fi
echo "k1-cosmic-session: cosmic-session exited with status $status"
exit "$status"
