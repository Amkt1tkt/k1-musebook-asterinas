#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: select the syslog fallback after journald finishes starting.

set -euo pipefail

query_attempts=10
query_timeout=2s
kill_after=1s

for ((attempt = 0; attempt < query_attempts; attempt++)); do
  if snapshot=$(timeout --kill-after="$kill_after" "$query_timeout" \
      systemctl show --all --no-pager \
      --property=LoadState,ActiveState,SubState,Job \
      systemd-journald.service 2>/dev/null); then
    load_state=
    active_state=
    sub_state=
    job=unknown
    while IFS='=' read -r property value; do
      case "$property" in
        LoadState) load_state=$value ;;
        ActiveState) active_state=$value ;;
        SubState) sub_state=$value ;;
        Job) job=$value ;;
      esac
    done <<< "$snapshot"

    # Job= is empty without a queued job. Failed/dead states before an
    # automatic restart are not final failures and must retain journald.
    if [[ $load_state == loaded && -z $job && -n $sub_state ]]; then
      case "$active_state/$sub_state" in
        active/running)
          exit 0
          ;;
        failed/failed | inactive/dead)
          exec timeout --kill-after="$kill_after" "$query_timeout" \
            systemctl --no-block start aster-syslog-kmsg-forwarder.service
          ;;
      esac
    fi
  fi

  if ((attempt + 1 < query_attempts)); then
    sleep 1
  fi
done

echo "aster-syslog-kmsg: unable to confirm journald startup state" >&2
exit 1
