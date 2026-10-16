#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: standalone compositor launcher for serial recovery and kiosk use.
# Embedded by cosmic.nix with the compositor and runtime tools on PATH.
# Optional arguments run as a kiosk child; exiting that child ends the compositor.

set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
  echo "k1-cosmic-compositor: must be root; libseat's builtin backend opens devices directly" >&2
  exit 1
fi

# tty1 becomes a graphics terminal. Route output to the serial console;
# K1_COSMIC_CONSOLE= keeps the caller's output instead.
console="${K1_COSMIC_CONSOLE-/dev/console}"
if [ -n "$console" ]; then
  exec >>"$console" 2>&1
fi

ensure_dir() {
  local path=$1 mode=$2
  mkdir -p "$path" && chmod "$mode" "$path" \
    || echo "k1-cosmic-compositor: failed to create $path mode $mode (non-fatal)" >&2
}

# Recreate volatile directories for manual starts without boot activation.
ensure_dir /run/user/0 0700
# Smithay prepares X11 sockets even when XWayland is not installed.
ensure_dir /tmp/.X11-unix 1777

# Inherited display variables select a nested compositor instead of KMS.
# Leave the explicit COSMIC_BACKEND override under the caller's control.
for stale in WAYLAND_DISPLAY WAYLAND_SOCKET DISPLAY; do
  if [[ -v $stale ]]; then
    echo "k1-cosmic-compositor: unsetting inherited $stale=${!stale} (selects a nested backend)"
    unset "$stale"
  fi
done
if [ -n "${COSMIC_BACKEND-}" ]; then
  echo "k1-cosmic-compositor: COSMIC_BACKEND=$COSMIC_BACKEND overrides backend selection"
fi

export XDG_RUNTIME_DIR=/run/user/0
export XDG_SESSION_TYPE=wayland
export XDG_CURRENT_DESKTOP=COSMIC
export HOME=/root
export LIBSEAT_BACKEND=builtin
export RUST_BACKTRACE=full
: "${RUST_LOG:=warn}"
export RUST_LOG
# The login environment supplies COSMIC_RENDER_DEVICE. KMS and rendering use
# separate DRM devices; selecting renderD128 enables dmabuf on the main socket.
echo "k1-cosmic-compositor: render device: ${COSMIC_RENDER_DEVICE-<unset; expected /dev/dri/renderD128>}"

# The input module is optional. Its system-profile helper initializes the
# udev database that libinput requires before accepting evdev devices.
if [ -x /run/current-system/sw/bin/seed-input-udev-db ]; then
  /run/current-system/sw/bin/seed-input-udev-db \
    || echo "k1-cosmic-compositor: seed-input-udev-db failed; libinput will skip uninitialized devices" >&2
fi

# Restore the text login after a manual compositor run. The automatic session
# uses systemd Conflicts= to manage tty1 instead.
stopped_getty=0
if systemctl is-active --quiet getty@tty1.service; then
  echo "k1-cosmic-compositor: stopping getty@tty1.service"
  if systemctl stop getty@tty1.service; then
    stopped_getty=1
  else
    echo "k1-cosmic-compositor: could not stop getty@tty1.service; tty1 will be contended" >&2
  fi
fi
trap 'if [ "$stopped_getty" = 1 ]; then systemctl start getty@tty1.service || true; fi' EXIT

# Keep the latest run in tmpfs and preserve the compositor's exit status
# through tee. A log creation failure must not prevent startup.
comp_log=/run/cosmic-comp.log
echo "k1-cosmic-compositor: starting compositor; log: $comp_log"
status=0
if : >"$comp_log"; then
  cosmic-comp "$@" 2>&1 | tee "$comp_log" || status=${PIPESTATUS[0]}
else
  echo "k1-cosmic-compositor: cannot create $comp_log; using console output only" >&2
  cosmic-comp "$@" || status=$?
fi
echo "k1-cosmic-compositor: cosmic-comp exited with status $status"
exit "$status"
