<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: maintenance rules for the board-local DRM crate. -->

# Maintenance conventions

This is a downstream crate. Each source file carries an SPDX header and one
`K1 MUSE Book` header; nested upstream-change fences are unnecessary for files
that have no upstream counterpart. Keep the safe Rust boundary intact.

The `uapi` definitions preserve Linux structure layout and ioctl encoding.
Changes must reconcile the Linux source, layout assertions, and kernel-adapter
dispatch. Retain explicit validation of reserved fields, flags, and arithmetic;
report unsupported operations instead of acknowledging work that was not done.

Hardware-independent protocols belong here. DPU MMIO belongs in framebuffer;
PowerVR firmware, MMU, and scheduling belong in PowerVR. Preserve one owner for
each hardware block. Temporary instrumentation must not add unconditional
updates or serial output to hot paths.

Follow the locking contracts in [ARCHITECTURE.md](ARCHITECTURE.md). Disabling
preemption does not defer bottom halves. Interrupt-disabled sections must not
allocate, sleep, wait on MMIO, or invoke arbitrary callbacks. Remove driver
objects from tables under their lock and release final references after
unlocking.

Regression tests should drive the production state machines and dispatcher with
controlled software drivers. A host harness must identify extracted production
code and the scheduler or MMIO boundaries it substitutes. Report host execution,
cross-compilation, QEMU execution, and hardware verification separately.
