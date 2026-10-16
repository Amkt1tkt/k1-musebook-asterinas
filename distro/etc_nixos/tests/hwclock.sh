#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: exercise generated clock helpers without changing any clock.
# Usage: hwclock.sh HCTOSYS_HELPER SYSTOHC_HELPER

set -euo pipefail
[[ $# == 2 ]] || { echo "usage: $0 HCTOSYS_HELPER SYSTOHC_HELPER" >&2; exit 2; }
boot_helper=$(realpath -- "$1")
stop_helper=$(realpath -- "$2")
# Check the known helper structure; this is not a sandbox for arbitrary scripts.
# Refuse calls that could bypass the fake function and reach a real clock.
if grep -Eq '[^[:space:]]*/hwclock([[:space:]]|$)|(command|exec)[[:space:]].*hwclock' "$boot_helper" "$stop_helper"; then
  echo 'helper contains a hwclock call that cannot be isolated safely' >&2
  exit 2
fi
real_date=$(command -v date)
real_timeout=$(command -v timeout)
test_dir=$(mktemp -d)
trap 'rm -rf -- "$test_dir"' EXIT

# BASH_ENV overrides only these two commands. The generated scripts and their
# runtime PATH remain unchanged; all clock writes are intercepted here.
cat > "$test_dir/commands" <<'STUB'
[[ $CLOCK_CASE_MODE != read-ignore-term ]] || trap '' TERM
hwclock() {
  printf '%s\n' "$*" >> "$CLOCK_CASE_DIR/calls"
  case "$*" in
    '--show --utc --noadjfile')
      case "$CLOCK_CASE_MODE" in
        missing) echo 'hwclock: cannot open rtc device' >&2; return 1 ;;
        read-error) echo 'hwclock: RTC read failed' >&2; return 1 ;;
        parse-error) echo 'not a calendar date'; return 0 ;;
        read-hang|read-ignore-term) while :; do :; done ;;
        warning) echo 'hwclock: diagnostic accompanying a valid reading' >&2 ;;
      esac
      printf '%s\n' "$CLOCK_CASE_SHOWN"
      ;;
    '--hctosys --utc --noadjfile')
      [[ $CLOCK_CASE_MODE != sync-error ]]
      ;;
    '--systohc --utc --noadjfile')
      case "$CLOCK_CASE_MODE" in
        missing|write-error) echo 'hwclock: RTC write failed' >&2; return 1 ;;
        write-hang) while :; do :; done ;;
      esac
      ;;
    *) echo "unexpected hwclock command: $*" >&2; return 99 ;;
  esac
}
date() {
  if [[ $* == '-u +%s' ]]; then
    [[ $CLOCK_CASE_MODE != date-error ]] || return 1
    printf '%s\n' "$CLOCK_CASE_EPOCH"
  else
    "$CLOCK_REAL_DATE" "$@"
  fi
}
STUB

checks=0
run_case() {
  local name=$1 helper=$2 mode=$3 shown=$4 epoch=$5 expected=$6
  local status=0 actual
  export CLOCK_CASE_DIR="$test_dir/$name" CLOCK_CASE_MODE="$mode"
  export CLOCK_CASE_SHOWN="$shown" CLOCK_CASE_EPOCH="$epoch"
  mkdir "$CLOCK_CASE_DIR"
  : > "$CLOCK_CASE_DIR/calls"
  BASH_ENV="$test_dir/commands" CLOCK_REAL_DATE="$real_date" \
    "$BASH" "$helper" > "$CLOCK_CASE_DIR/stdout" 2> "$CLOCK_CASE_DIR/stderr" || status=$?
  actual=$(cat "$CLOCK_CASE_DIR/calls")
  if [[ $status != 0 || $actual != "$expected" ]]; then
    printf '%s: status=%s\nexpected calls:\n%s\nactual calls:\n%s\n' \
      "$name" "$status" "$expected" "$actual" >&2
    cat "$CLOCK_CASE_DIR/stdout" "$CLOCK_CASE_DIR/stderr" >&2
    exit 1
  fi
  if [[ $mode == warning ]]; then
    grep -q 'diagnostic accompanying a valid reading' "$CLOCK_CASE_DIR/stderr"
  fi
  checks=$((checks + 1))
}

show='--show --utc --noadjfile'
sync=$'--show --utc --noadjfile\n--hctosys --utc --noadjfile'
write='--systohc --utc --noadjfile'
valid='2026-01-02 00:00:00.000000+00:00'
run_case no-device "$boot_helper" missing "$valid" 1767312000 "$show"
run_case read-failure "$boot_helper" read-error "$valid" 1767312000 "$show"
run_case parse-failure "$boot_helper" parse-error "$valid" 1767312000 "$show"
run_case old-rtc "$boot_helper" ok '2019-12-31 23:59:59+00:00' 1767312000 "$show"
run_case rtc-boundary "$boot_helper" ok '2020-01-01 00:00:00+00:00' 1767312000 "$sync"
run_case valid-rtc "$boot_helper" ok "$valid" 1767312000 "$sync"
run_case local-east "$boot_helper" ok '2026-01-02 08:00:00+08:00' 1767312000 "$sync"
run_case local-west "$boot_helper" ok '2026-01-01 19:00:00-05:00' 1767312000 "$sync"
run_case old-rtc-east "$boot_helper" ok '2020-01-01 07:59:59+08:00' 1767312000 "$show"
run_case rtc-warning "$boot_helper" warning "$valid" 1767312000 "$sync"
run_case sync-failure "$boot_helper" sync-error "$valid" 1767312000 "$sync"
run_case system-read-failure "$stop_helper" date-error "$valid" 1767312000 ''
run_case old-system "$stop_helper" ok "$valid" 1577836799 ''
run_case system-boundary "$stop_helper" ok "$valid" 1577836800 "$write"
run_case valid-system "$stop_helper" ok "$valid" 1767312000 "$write"
run_case write-failure "$stop_helper" write-error "$valid" 1767312000 "$write"
run_case missing-write-device "$stop_helper" missing "$valid" 1767312000 "$write"

# These are actual GNU timeout invocations, with shorter test-only deadlines.
# No production timeout or helper text is replaced.
for direction in read write read-ignore-term; do
  helper=$boot_helper
  [[ $direction != write ]] || helper=$stop_helper
  export CLOCK_CASE_DIR="$test_dir/$direction-timeout" CLOCK_CASE_MODE="$direction-hang"
  export CLOCK_CASE_SHOWN="$valid" CLOCK_CASE_EPOCH=1767312000
  [[ $direction != read-ignore-term ]] || CLOCK_CASE_MODE=read-ignore-term
  mkdir "$CLOCK_CASE_DIR"
  status=0
  { "$real_timeout" --kill-after=1s 0.2s env \
    BASH_ENV="$test_dir/commands" CLOCK_REAL_DATE="$real_date" \
    "$BASH" "$helper" > "$CLOCK_CASE_DIR/stdout" 2> "$CLOCK_CASE_DIR/stderr" || status=$?
  } 2> "$CLOCK_CASE_DIR/timeout-status"
  expected_status=124
  [[ $direction != read-ignore-term ]] || expected_status=137
  [[ $status == "$expected_status" ]] || { echo "$direction timeout returned $status" >&2; exit 1; }
  checks=$((checks + 1))
done
printf '%s isolated clock-helper cases passed; no real clock was changed.\n' "$checks"
