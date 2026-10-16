#!/usr/bin/env bash
# SPDX-License-Identifier: MPL-2.0
# K1 MUSE Book: isolated regressions for the input udev database seeder.
# Run with Bash on a build host; only temporary input and database trees are used.

set -euo pipefail

test_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
seed="$test_dir/../modules/input-udev-seed.sh"
work=$(mktemp -d "${TMPDIR:-/tmp}/input-udev-seed-test.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT

mkdir -p "$work/source/class" "$work/source/db"
export INPUT_UDEV_CLASS_DIR="$work/source/class"
export INPUT_UDEV_DB_DIR="$work/source/db"
# shellcheck source=../modules/input-udev-seed.sh
source "$seed"

passed=0
failed=0
expect() {
  local name=$1
  shift
  if "$@"; then
    printf 'PASS %s\n' "$name"
    passed=$((passed + 1))
  else
    printf 'FAIL %s\n' "$name" >&2
    failed=$((failed + 1))
  fi
}

reject() {
  local name=$1
  shift
  if "$@"; then
    printf 'FAIL %s\n' "$name" >&2
    failed=$((failed + 1))
  else
    printf 'PASS %s\n' "$name"
    passed=$((passed + 1))
  fi
}

# Bitmap helpers must handle shortened words, high-first word order and bit 63.
_bm=(7)
expect 'bitmap: EV_KEY in short word' bm_has 1
expect 'bitmap: EV_REL in short word' bm_has 2
reject 'bitmap: EV_ABS absent' bm_has 3
_bm=(10000000)
expect 'bitmap: KEY_ENTER' bm_has 28
_bm=(10000 0 0 0 0)
expect 'bitmap: BTN_MOUSE in high word' bm_has 272
reject 'bitmap: BTN_RIGHT absent' bm_has 273
_bm=(1)
expect 'bitmap: bit zero' bm_has 0
reject 'bitmap: omitted high word' bm_has 64
_bm=(8000000000000000)
expect 'bitmap: bit 63 without signed arithmetic overflow' bm_has 63
_bm=(1 0)
expect 'bitmap: bit 64 in the next word' bm_has 64

for tag in KEYBOARD MOUSE TOUCHPAD SWITCH; do
  expect "tags: $tag accepted" db_has_input_tags \
    <<< $'E:ID_INPUT=1\n'"E:ID_INPUT_$tag=1"
done
reject 'tags: ID_INPUT alone rejected' db_has_input_tags \
  <<< 'E:ID_INPUT=1'
reject 'tags: dimensions are not a type tag' db_has_input_tags \
  <<< $'E:ID_INPUT=1\nE:ID_INPUT_WIDTH_MM=100'
reject 'tags: type alone rejected' db_has_input_tags \
  <<< 'E:ID_INPUT_MOUSE=1'

reject 'tags: final ID_INPUT=0 overrides an earlier 1' db_has_input_tags \
  <<< $'E:ID_INPUT=1\nE:ID_INPUT=0\nE:ID_INPUT_MOUSE=1'
expect 'tags: final ID_INPUT=1 overrides an earlier 0' db_has_input_tags \
  <<< $'E:ID_INPUT=0\nE:ID_INPUT=1\nE:ID_INPUT_MOUSE=1'
reject 'tags: final type=0 overrides an earlier 1' db_has_input_tags \
  <<< $'E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_MOUSE=0'
expect 'tags: final type=1 overrides an earlier 0' db_has_input_tags \
  <<< $'E:ID_INPUT=1\nE:ID_INPUT_MOUSE=0\nE:ID_INPUT_MOUSE=1'
expect 'tags: another active type remains sufficient' db_has_input_tags \
  <<< $'E:ID_INPUT=1\nE:ID_INPUT_KEY=1\nE:ID_INPUT_MOUSE=1\nE:ID_INPUT_MOUSE=0'

new_fixture() {
  fixture=$(mktemp -d "$work/case with spaces.XXXXXXXX")
  class="$fixture/class"
  db="$fixture/db"
  stub_bin="$fixture/bin"
  mkdir -p "$class" "$db" "$stub_bin"
  mode=ok
  cat > "$stub_bin/udevadm" <<'EOF'
#!/usr/bin/env bash
case $UDEVADM_MODE in
  ok) printf '%s\n' 'ID_INPUT=1' 'ID_INPUT_MOUSE=1' 'IGNORED=value' ;;
  invalid) printf '%s\n' 'ID_INPUT=1' 'ID_INPUT_WIDTH_MM=100' ;;
  fail) exit 1 ;;
  *) exit 2 ;;
esac
EOF
  chmod +x "$stub_bin/udevadm"
}

add_event() {
  local event=$1 id=$2
  mkdir -p "$class/$event"
  printf '%s\n' "$id" > "$class/$event/dev"
}

set_capabilities() {
  local event=$1 ev=$2 key=$3 rel=$4 abs=$5
  local caps="$class/$event/device/capabilities"
  mkdir -p "$caps"
  printf '%s\n' "$ev" > "$caps/ev"
  printf '%s\n' "$key" > "$caps/key"
  printf '%s\n' "$rel" > "$caps/rel"
  printf '%s\n' "$abs" > "$caps/abs"
}

run_seed() {
  status=0
  INPUT_UDEV_CLASS_DIR="$class" INPUT_UDEV_DB_DIR="$db" \
    UDEVADM_MODE="$mode" PATH="$stub_bin:$PATH" \
    bash "$seed" "$@" > "$fixture/out" 2> "$fixture/err" || status=$?
}

expect_status() {
  expect "$1" test "$status" -eq "$2"
}

expect_no_tmp() {
  expect "$1" test -z "$(find "$db" -name '.tmp.*' -print)"
}

new_fixture
add_event event9 42:6
run_seed
expect_status 'udevadm: success' 0
printf 'E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\n' > "$fixture/expected"
expect 'dev attribute determines database filename' cmp -s "$fixture/expected" "$db/c42:6"
reject 'event name is not used as device number' test -e "$db/c13:9"
expect 'database file permissions are 0644' test "$(stat -c %a "$db/c42:6")" = 644
expect_no_tmp 'successful write leaves no temporary file'
run_seed
expect_status 'repeated seed: success' 0
expect 'repeated seed preserves exact content and final newline' cmp -s "$fixture/expected" "$db/c42:6"
expect_no_tmp 'repeated seed leaves no temporary file'
run_seed --check
expect_status 'valid database: check success' 0
expect 'check leaves the database unchanged' cmp -s "$fixture/expected" "$db/c42:6"

# Exercise each K1 fallback independently of the installed host udevadm.
for mode_case in fail invalid; do
  new_fixture
  mode=$mode_case
  add_event event0 13:0
  set_capabilities event0 2 10000002 0 0
  run_seed
  expect_status "udevadm $mode_case: sysfs keyboard fallback" 0
  expect "udevadm $mode_case: keyboard tag" grep -qx 'E:ID_INPUT_KEYBOARD=1' "$db/c13:0"
  expect "udevadm $mode_case: key tag" grep -qx 'E:ID_INPUT_KEY=1' "$db/c13:0"
  expect "udevadm $mode_case: fallback reported" grep -q '(sysfs)' "$fixture/out"
done

new_fixture
mode=fail
add_event event0 13:0
set_capabilities event0 6 '10000 0 0 0 0' 3 0
run_seed
expect_status 'sysfs: relative mouse' 0
expect 'sysfs: mouse tag' grep -qx 'E:ID_INPUT_MOUSE=1' "$db/c13:0"

new_fixture
mode=fail
add_event event0 13:0
set_capabilities event0 a '420 0 0 0 0 0' 0 3
run_seed
expect_status 'sysfs: absolute touchpad' 0
expect 'sysfs: touchpad tag' grep -qx 'E:ID_INPUT_TOUCHPAD=1' "$db/c13:0"

new_fixture
mode=fail
add_event event0 13:0
set_capabilities event0 20 0 0 0
run_seed
expect_status 'sysfs: lid switch' 0
expect 'sysfs: switch tag' grep -qx 'E:ID_INPUT_SWITCH=1' "$db/c13:0"

new_fixture
mode=fail
add_event event0 13:0
set_capabilities event0 2 '10000000000000 0' 0 0
run_seed
expect_status 'sysfs: power key' 0
expect 'sysfs: power key is tagged as a key' grep -qx 'E:ID_INPUT_KEY=1' "$db/c13:0"
reject 'sysfs: power key is not a full keyboard' grep -q 'ID_INPUT_KEYBOARD' "$db/c13:0"

new_fixture
mode=fail
add_event event0 13:0
set_capabilities event0 0 0 0 0
run_seed
expect_status 'unclassifiable device fails' 1
reject 'unclassifiable device has no database record' test -e "$db/c13:0"

new_fixture
add_event event0 13:0
for contents in missing empty input-only type-only width-only malformed no-newline; do
  rm -f -- "$db/c13:0"
  case $contents in
    missing) ;;
    empty) : > "$db/c13:0" ;;
    input-only) printf 'E:ID_INPUT=1\n' > "$db/c13:0" ;;
    type-only) printf 'E:ID_INPUT_MOUSE=1\n' > "$db/c13:0" ;;
    width-only) printf 'E:ID_INPUT=1\nE:ID_INPUT_WIDTH_MM=100\n' > "$db/c13:0" ;;
    malformed) printf 'E:ID_INPUT=1\nE:ID_INPUT_MOUSE=true\n' > "$db/c13:0" ;;
    no-newline) printf 'E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1' > "$db/c13:0" ;;
  esac
  run_seed --check
  expect_status "check rejects $contents database" 1
done

new_fixture
add_event event0 13:0
for property in ID_INPUT ID_INPUT_MOUSE; do
  for last_value in 0 1; do
    printf 'E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\nE:%s=%s\n' \
      "$property" "$last_value" > "$db/c13:0"
    run_seed --check
    expect_status "check honors final $property=$last_value" "$((1 - last_value))"
  done
done

new_fixture
add_event event0 13:0
rmdir "$db"
printf 'keep\n' > "$db"
run_seed
expect_status 'database root must be a directory' 1
expect 'bad database root is preserved' grep -qx keep "$db"

new_fixture
add_event event0 13:0
mkdir "$db/c13:0"
directory_mode=$(stat -c %a "$db/c13:0")
printf 'keep\n' > "$db/c13:0/keep"
run_seed
expect_status 'destination directory is rejected' 1
expect 'destination directory contents are preserved' grep -qx keep "$db/c13:0/keep"
expect 'destination directory permissions are preserved' test "$(stat -c %a "$db/c13:0")" = "$directory_mode"
expect_no_tmp 'destination directory failure leaves no temporary file'

# A conditional caller disables errexit in shell functions. Fail each write
# step explicitly; successful logs must not conceal an error or replace old data.
for command in mkdir mktemp printf chmod mv; do
  new_fixture
  add_event event0 13:0
  printf 'E:ID_INPUT=1\nE:ID_INPUT_KEYBOARD=1\n' > "$db/c13:0"
  cp "$db/c13:0" "$fixture/original"
  if [ "$command" = printf ]; then
    status=0
    INPUT_UDEV_CLASS_DIR="$class" INPUT_UDEV_DB_DIR="$db" \
      UDEVADM_MODE="$mode" PATH="$stub_bin:$PATH" \
      bash -c '
        source "$1"
        fault_write_done=0
        printf() {
          if [[ $# -ge 2 && $1 == "%s\n" && $2 == E:ID_INPUT=* && $fault_write_done == 0 ]]; then
            fault_write_done=1
            builtin printf "partial\n"
            return 73
          fi
          builtin printf "$@"
        }
        main
      ' _ "$seed" > "$fixture/out" 2> "$fixture/err" || status=$?
  else
    printf '#!/usr/bin/env bash\nexit 73\n' > "$stub_bin/$command"
    chmod +x "$stub_bin/$command"
    run_seed
  fi
  expect_status "$command failure is propagated" 1
  expect "$command failure preserves the old record" cmp -s "$fixture/original" "$db/c13:0"
  expect_no_tmp "$command failure leaves no temporary file"
  reject "$command failure does not report success" grep -q 'seed: 1 device(s) ok' "$fixture/out"
done

new_fixture
add_event event0 bad
add_event event1 13:8
run_seed
expect_status 'one bad node fails the aggregate run' 1
expect 'a later good node is still processed' test -f "$db/c13:8"
expect 'aggregate failure names its device count' grep -q '1 of 2 device(s) failed' "$fixture/out"

new_fixture
mkdir "$class/event0"
run_seed
expect_status 'missing dev attribute fails' 1

new_fixture
run_seed
expect_status 'empty input class is nonfatal' 0
rmdir "$class"
run_seed
expect_status 'missing input class is nonfatal' 0

new_fixture
for argument in --help -h; do
  run_seed "$argument"
  expect_status "$argument succeeds" 0
  expect "$argument prints usage" grep -q 'seed-input-udev-db' "$fixture/out"
done
for argument in --unknown --self-test; do
  run_seed "$argument"
  expect_status "$argument is rejected" 2
done
for argument in --check --help; do
  run_seed "$argument" unexpected
  expect_status "$argument rejects an extra argument" 2
done

printf 'input-udev-seed: %d passed, %d failed\n' "$passed" "$failed"
[ "$failed" -eq 0 ]
