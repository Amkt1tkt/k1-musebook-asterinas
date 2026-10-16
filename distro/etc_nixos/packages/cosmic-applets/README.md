<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: pinned COSMIC battery subscription compatibility. -->

The pinned battery applet initializes before the optional UPower bridge may be
ready. `upower-initialization.patch` retries failed initialization every two
seconds and logs the first failure. A successful subscription emits its current
battery state before waiting for changes. Failed device queries trigger a retry;
only a successful enumeration without a power-supply battery emits `NoBattery`.

`apply_vendor.py` checks the original files before patching the writable Cargo
vendor copy, then installs a complete source-checksum manifest. It does not change
the source pin, dependency pin, or Cargo lockfile.

The native regressions use a private D-Bus and the patched subscription itself:

```sh
python3 tests/run.py \
  --vendor "$COSMIC_APPLETS_VENDOR" \
  --work-dir "$NATIVE_TEST_DIR" \
  --dbus-daemon "$NATIVE_DBUS_DAEMON"
```

Use the vendor directory from this recipe's pinned `cargoDeps`, a fresh native
test directory, and a native `dbus-daemon`. The fixture builds offline with the
vendored dependency versions. It checks late service readiness, the initial
snapshot and later changes, recovery from failed device queries, and successful
no-battery enumeration. `--upstream` runs the same tests against the unpatched
subscription; the first three expose the regressions. Cross builds keep
`doCheck = false` because the build host cannot execute RISC-V tests.
