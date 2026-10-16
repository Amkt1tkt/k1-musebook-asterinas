<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: PowerVR driver contracts and maintenance. -->

# PowerVR maintenance conventions

These conventions cover the driver and host generator.
The repository's [AGENTS.md](../../../../../AGENTS.md) defines general contribution rules.

## Safe Rust and generated definitions

Kernel code retains `#![deny(unsafe_code)]` and does not introduce C or FFI.
MMIO, DMA, cache maintenance and memory translation use OSTD's safe interfaces.
A missing low-level primitive belongs in its owning layer.

Regenerate `rogue/{cr,defs,fwif,heap,mips,mmu,sf}.rs` through the generator, not by hand.
`rogue/mod.rs` is handwritten.
Use the 18 headers at the UPSTREAM revision and run `--check` after generation.
Both write and check paths use the same pinned rustfmt.

The parser implements only the expressions, structures and preprocessing needed by the fixed headers.
`parse_evaluable_defines` may omit unevaluable definitions outside the selected subset.
Allow-lists, mandatory parse errors and coverage tests protect selected registers, constants, layouts and trace entries.
A source revision change requires reviewing this coverage again.
The fetch URL pins the commit, but an existing local cache still requires provenance verification.

## ABI and licensing

Firmware structures use `repr(C)`, explicit padding where needed, `Pod`, and size/offset assertions.
The four pinned layout-check headers contribute 607 executable assertions.
CPU physical, Rogue virtual and MIPS firmware addresses are distinct types and must remain distinct.

Ported definitions retain `MIT OR GPL-2.0-only`, Imagination copyright notices and local MIT license text.
The generator uses MPL-2.0.
Source modules identify Linux counterparts and UPSTREAM pins their revision.
New board files have an English K1 purpose header.
Firmware is governed by `LICENSE.powervr`; its OPEN_SOURCE protocol flag makes no licensing claim.

## Input validation and errors

Map `PvrError` to `DrmError` in `error.rs`, with deliberate differences documented in [ABI.md](ABI.md).
Invalid user data and firmware images return errors rather than panicking.

Preserve UAPI field order, `repr(C)`, padding and layout assertions.
Versioned arrays use only `uapi::obj_array`: short elements are zero-extended;
unknown tails of longer elements must be zero.
Bound counts and total byte sizes, and reject unknown flags, overflow and invalid per-file handles.

Command streams follow `pvr_stream_defs.c` field order, feature conditions, widths and lengths.
Reject unknown extensions and validate the entire batch before publishing user-visible work.
Memory safety alone does not validate a GPU command stream.

## Verification and diagnostics

Behavior changes require reproducible inputs and assertions against the affected contract.
Generator tests, kernel tests, bare ioctl probes and real GPU execution validate different layers.
Allocation-failure tests must check resources and unrelated owners, not just errno.

Comments describe ownership, hardware constraints and failure behavior in English.
Unsupported paths return errors instead of `todo!` or user-triggered panic.
Diagnostics must avoid per-ioctl global locks, continuous UART output and unbounded dumps.
Detailed firmware reports require an explicit diagnostic option.

## K1 DMA contract

- GPU-visible write-back memory and page tables use `DmaStream` with explicit synchronization;
  `DmaCoherent` must not imply hardware coherence.
- Firmware-written polling words use `FwCache::POLLED` / `alloc_uncached` with CPU PBMT_NC.
  Their sync operation is a successful no-op; do not issue cache-block operations on uncached aliases.
  MIPS PTE cache attributes and CPU attributes are independent.
- Images, KCCB/CCCB rings and page tables remain write-back with synchronization.
  Choose FWCCB/control attributes by actual readers and writers, not by the name "ring".
- Rogue TLB flush flags accumulate until serialized firmware acknowledgement; CPU cache cleaning does not replace the flush.
- Uncached control writes still require `ostd::mm::dma::dma_io_fence()` before an MMIO doorbell.
  Cache maintenance, CPU/I/O ordering and confirmation that DMA stopped are separate contracts.
- BOs allocate contiguous backing eagerly. CPU faults locate existing pages and must not allocate whole BOs.
- PBMT_NC requires enabled Svpbmt/SBI PBMTE support; mapping flags alone do not establish that capability.
