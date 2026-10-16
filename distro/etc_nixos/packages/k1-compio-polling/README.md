<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: pinned compio polling compatibility and verification. -->

# compio polling backend

The pinned COSMIC Files, Edit and Terminal applications use compio 0.14.0.
Asterinas does not implement `io_uring`; the original default backend cannot
initialize its runtime. This RISC-V adapter selects compio's `polling` backend,
using existing file syscalls and event polling. It requires no kernel syscall
addition or io-uring architecture-check bypass.

`default.nix` preserves packages on other architectures. For RISC-V it patches
the writable vendor copy, covering both direct and transitive compio users.
Settings and the command-line clients do not use this adapter.

`apply_vendor.py` verifies the original manifest and each application's complete
lockfile SHA256. It changes only compio's default features, removes the inactive
io-uring dependency edge/package from both source and vendor lockfiles, and
retains every remaining version, source and package checksum. It performs no
Cargo update or network resolution. A changed source pin must be reviewed before
updating these checks. All listed file checksums are validated before writing;
the compio checksum manifest is then rebuilt without altering other crates.

The adapter composes with `k1-softbuffer-damage`, which patches another crate.
Native regression must force `io_uring_setup` to return `ENOSYS`, demonstrate the
original initialization failure, and exercise actual compio create/read/write,
sync, copy, rename and unlink operations with polling. The offline RISC-V feature
graph must include `compio-driver/polling` and exclude active io-uring. Native
checks and cross builds do not replace K1 file-operation and full GUI testing.
