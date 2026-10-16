<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: PowerVR driver contracts and maintenance. -->

# PowerVR BXE-2-32 driver

`k1-musebook-pvr` provides the K1 MUSE Book's PowerVR render driver in safe Rust.
It is a board component outside upstream Asterinas and retains `#![deny(unsafe_code)]`.
The driver manages GPU power, MIPS firmware, page tables, submission queues and rendering resources.
It registers a `powervr` render node; the DPU owns display scanout.
Generic DRM, GEM, PRIME and fence support belongs to the [DRM component](../drm/README.md).

See [architecture](ARCHITECTURE.md), [ABI](ABI.md) and [maintenance conventions](CONVENTIONS.md).
Repository rules are in [AGENTS.md](../../../../../AGENTS.md).

## Source and firmware

The port follows Linux `drivers/gpu/drm/imagination` and `include/uapi/drm/pvr_drm.h`
at the commit recorded in [UPSTREAM](UPSTREAM).
Ported code is licensed under `MIT OR GPL-2.0-only`; see [LICENSE-MIT](LICENSE-MIT).
The host generator is licensed under MPL-2.0.

Firmware is a separate binary governed by `LICENSE.powervr`.
Permission to redistribute the binary does not imply access to firmware source.
`PVR_FW_FLAGS_OPEN_SOURCE` selects a driver protocol, not a copyright license.
The Nix firmware package supplies `powervr/rogue_36.29.52.182_v1.fw` for BVNC `36.29.52.182`.
Its MIPS image uses info version 3 and firmware version 1.1, build 6976702.
Replacing the image requires validating its device identity and protocol again.

## Regeneration and tests

The seven generated files are `src/rogue/{cr,defs,fwif,heap,mips,mmu,sf}.rs`.
`src/rogue/mod.rs` is handwritten.
At the pinned revision, the generated ABI contains 607 size/offset assertions and 775 trace-format entries.
The parser deliberately supports the selected header subset; it is not a general C compiler.

Run from the repository root with the pinned Rust toolchain and rustfmt.
The input directory must contain headers verified against the UPSTREAM commit.

```sh
PVR_REGEN_HEADERS=/path/to/verified/headers cargo test \
  --manifest-path kernel/core/comps/k1-musebook/pvr/regen/Cargo.toml \
  --target x86_64-unknown-linux-gnu -- --include-ignored
cargo run --manifest-path kernel/core/comps/k1-musebook/pvr/regen/Cargo.toml \
  --target x86_64-unknown-linux-gnu -- \
  --headers /path/to/verified/headers --check
```

Without `--include-ignored`, seven header-dependent tests are skipped and four parser/manifest tests run.
Missing headers fail regeneration or an explicitly enabled header-dependent test.
Normal kernel builds consume committed output and do not run the generator.

Kernel tests use the repository's OSDK environment.
Synthetic registers, firmware fixtures and failure injection exercise layouts, rollback and error propagation.
They do not validate hardware timing, rendered output or sustained GPU operation.
User-space probes are under `test/initramfs/src/regression/device/drm/`
and `test/initramfs/src/regression/dynamic_linking/drm_probe/`.

## Runtime limits

BO backing is physically contiguous and allocated eagerly; only CPU VMA population is lazy.
Explicit DMA synchronization and firmware cache attributes are described in [CONVENTIONS](CONVENTIONS.md#k1-dma-contract).
Imported dma-bufs require explicit client synchronization; implicit reservation-fence synchronization is not implemented.

Timeouts, the watchdog and autosuspend are operational mechanisms.
Detailed firmware trace and stall reports require `k1.diag=gpu` and have bounded output.
Low-frequency resource counters describe ownership; they do not establish hardware performance or leak causes.
