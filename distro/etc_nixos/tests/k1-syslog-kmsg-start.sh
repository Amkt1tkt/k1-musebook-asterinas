#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: isolated startup selection checks with no service or socket access.

set -euo pipefail

test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT
helper=$(cd -- "$(dirname -- "$0")/../modules" && pwd)/k1-syslog-kmsg-start.sh
mkdir "$test_dir/bin"
export CASE_DIR PATH="$test_dir/bin:$PATH"

cat > "$test_dir/bin/systemctl" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
if [[ $1 == show ]]; then
  [[ $* == 'show --all --no-pager --property=LoadState,ActiveState,SubState,Job systemd-journald.service' ]]
  count=$(cat "$CASE_DIR/queries")
  count=$((count + 1))
  echo "$count" > "$CASE_DIR/queries"
  state=$(sed -n "${count}p" "$CASE_DIR/states")
  [[ -n $state ]] || state=$(tail -n 1 "$CASE_DIR/states")
  case "$state" in
    query-error) exit 1 ;;
    missing-job)
      printf '%s\n' LoadState=loaded ActiveState=inactive SubState=dead
      exit 0 ;;
    missing-load)
      printf '%s\n' ActiveState=inactive SubState=dead Job=
      exit 0 ;;
  esac
  IFS='|' read -r load active sub job <<< "$state"
  printf '%s\n' "LoadState=$load" "ActiveState=$active" "SubState=$sub" "Job=$job"
else
  [[ $* == '--no-block start aster-syslog-kmsg-forwarder.service' ]]
  echo start >> "$CASE_DIR/starts"
  exit "$(cat "$CASE_DIR/start-status")"
fi
STUB

cat > "$test_dir/bin/timeout" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
[[ $1 == --kill-after=1s && $2 == 2s ]]
shift 2
echo timeout >> "$CASE_DIR/timeouts"
if [[ $2 == show && $(cat "$CASE_DIR/timeout-mode") == query ]]; then
  exit 124
fi
if [[ $2 == --no-block && $(cat "$CASE_DIR/timeout-mode") == start ]]; then
  exit 124
fi
exec "$@"
STUB
printf '#!/bin/sh\nexit 0\n' > "$test_dir/bin/sleep"
chmod +x "$test_dir/bin/"*

checks=0
run_case() {
  local name=$1 expected_status=$2 expected_starts=$3 expected_queries=$4
  local timeout_mode=$5 start_status=$6 status=0 starts=0
  shift 6
  CASE_DIR="$test_dir/$name"
  mkdir "$CASE_DIR"
  printf '%s\n' "$@" > "$CASE_DIR/states"
  echo 0 > "$CASE_DIR/queries"
  echo "$timeout_mode" > "$CASE_DIR/timeout-mode"
  echo "$start_status" > "$CASE_DIR/start-status"
  bash "$helper" > "$CASE_DIR/stdout" 2> "$CASE_DIR/stderr" || status=$?
  [[ ! -e $CASE_DIR/starts ]] || starts=$(wc -l < "$CASE_DIR/starts")
  if [[ $status != "$expected_status" || $starts != "$expected_starts" ||
        $(cat "$CASE_DIR/queries") != "$expected_queries" ]]; then
    echo "$name: unexpected status=$status starts=$starts queries=$(cat "$CASE_DIR/queries")" >&2
    cat "$CASE_DIR/stderr" >&2
    exit 1
  fi
  if [[ $expected_status == 1 && $expected_queries == 10 ||
        $timeout_mode == query ]]; then
    [[ $(wc -l < "$CASE_DIR/stderr") == 1 ]]
    grep -q 'unable to confirm journald startup state' "$CASE_DIR/stderr"
  fi
  checks=$((checks + 1))
}

run_case healthy 0 0 1 none 0 'loaded|active|running|'
run_case failed 0 1 1 none 0 'loaded|failed|failed|'
run_case stopped 0 1 1 none 0 'loaded|inactive|dead|'
run_case starting 1 0 10 none 0 'loaded|activating|start|'
run_case retrying 1 0 10 none 0 'loaded|activating|auto-restart|'
run_case failed-before-retry 1 0 10 none 0 'loaded|failed|failed-before-auto-restart|'
run_case dead-before-retry 1 0 10 none 0 'loaded|inactive|dead-before-auto-restart|'
run_case active-job 1 0 10 none 0 'loaded|active|running|19'
run_case active-dead 1 0 10 none 0 'loaded|active|dead|'
run_case active-failed 1 0 10 none 0 'loaded|active|failed|'
run_case failed-job 1 0 10 none 0 'loaded|failed|failed|19'
run_case stopped-job 1 0 10 none 0 'loaded|inactive|dead|19'
run_case query-error 1 0 10 none 0 query-error
run_case missing-job 1 0 10 none 0 missing-job
run_case missing-load 1 0 10 none 0 missing-load
run_case missing-substate 1 0 10 none 0 'loaded|inactive||'
run_case masked 1 0 10 none 0 'masked|inactive|dead|'
run_case not-found 1 0 10 none 0 'not-found|inactive|dead|'
run_case healthy-after-wait 0 0 2 none 0 'loaded|activating|start|' 'loaded|active|running|'
run_case failed-after-wait 0 1 2 none 0 'loaded|activating|auto-restart|' 'loaded|failed|failed|'
run_case job-completed 0 0 2 none 0 'loaded|active|running|19' 'loaded|active|running|'
run_case rpc-error-recovered 0 1 2 none 0 query-error 'loaded|inactive|dead|'
run_case start-rejected 5 1 1 none 5 'loaded|failed|failed|'
run_case query-timeout 1 0 0 query 0 'loaded|failed|failed|'
run_case start-timeout 124 0 1 start 0 'loaded|failed|failed|'

echo "$checks isolated syslog startup checks passed"
