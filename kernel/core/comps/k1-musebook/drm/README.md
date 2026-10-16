<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: board-local DRM/KMS core documentation. -->

# K1 MUSE Book DRM/KMS

`k1-musebook-drm` implements a subset of the Linux DRM user-space interface in
safe Rust. It is shared by the software device, the K1 display controller, and
PowerVR. The crate denies unsafe code and does not depend on the PowerVR driver.

| Module | Responsibility |
| --- | --- |
| `device`, `file`, `ioctl` | Device registration, file permissions, per-open state, and argument dispatch. |
| `gem`, `prime` | Buffer ownership, mapping lifetime, cross-device dma-buf identity, and implicit fences. |
| `kms` | Display objects, properties, legacy-to-atomic translation, commits, and completion events. |
| `fence` | Completion state, interruptible waits, binary and timeline sync objects, and eventfd notifications. |
| `uapi` | Hand-maintained Linux ABI definitions with layout assertions and source references. |
| `drivers/soft` | A hardware-independent device for object and ioctl regression tests. |

The character-device and descriptor adapters are in
`kernel/core/src/device/dri.rs` and `dri_export.rs`. DPU register access belongs to
`kernel/core/comps/framebuffer/src/arch/riscv/k1_kms.rs`; this crate performs no
DPU or GPU MMIO.

See [architecture](ARCHITECTURE.md), [maintenance conventions](CONVENTIONS.md),
and [ABI scope](ABI.md). PowerVR firmware and generated interfaces are documented
in the [PowerVR driver](../pvr/README.md).

Kernel tests cover ioctl permissions, GEM lifetime, PRIME identity, KMS commits,
event budgets, and fence/sync-object protocols. User-space regressions are in
`test/initramfs/src/regression/device/drm/`. Compilation does not execute these
tests. Software-device tests do not establish DPU timing, GPU completion, DMA
coherency, or recovery on hardware.
