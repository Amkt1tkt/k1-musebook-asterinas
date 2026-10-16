<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: native integration tests for the UPower battery bridge. -->

The sysfs parser tests run with `cargo test --locked`. The integration tests
in `startup.rs` launch the shim on an isolated D-Bus bus and exercise delayed
BAT0, delayed bus startup, unreadable samples, recovery, and bounded logging.
They are ignored by default because they require a native `dbus-daemon`.

From the package directory, run:

```sh
UPOWER_SHIM_TEST_DBUS_DAEMON="$(command -v dbus-daemon)" \
    cargo test --locked --test startup -- --ignored
```

The daemon must match the host architecture; these tests do not run under a
cross build. Each test creates a private bus and temporary BAT0 files. Child
processes and files are cleaned up on success and on assertion failure.
