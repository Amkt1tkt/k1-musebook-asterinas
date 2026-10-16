<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: PowerVR driver contracts and maintenance. -->

# PowerVR ABI and port limits

The authoritative UAPI is `include/uapi/drm/pvr_drm.h` at the revision in [UPSTREAM](UPSTREAM).
Generic DRM/KMS, syncobj and PRIME contracts are maintained in the [DRM ABI](../drm/ABI.md).

## Device identity

The DPU primary node is `/dev/dri/card0` (226:0, driver `spacemit`).
The GPU render node is `/dev/dri/renderD128` (226:128, driver `powervr`).
PVR owns no primary node or scanout; clients pass rendering results to the DPU through PRIME/dma-buf.
The loader expects `powervr`, not the older DDK name `pvr`.

The platform identities must also remain distinct so libdrm does not merge the devices:
`/soc/dpu@c0340000` and `/soc/imggpu@cac00000`.
The QEMU software driver uses `/soft/aster-drm`.
Kernel `device/dri.rs` and each driver's `platform_identity` publish these identities.

## Private ioctls

All 14 commands have `DRM_RENDER_ALLOW`, require no DRM master, and operate through the render node.
Numbers below are relative to `DRM_COMMAND_BASE` (0x40).

| Number | Command | Direction |
| --- | --- | --- |
| `0x00` | `DEV_QUERY` | IOWR |
| `0x01` | `CREATE_BO` | IOWR |
| `0x02` | `GET_BO_MMAP_OFFSET` | IOWR |
| `0x03` | `CREATE_VM_CONTEXT` | IOWR |
| `0x04` | `DESTROY_VM_CONTEXT` | IOW |
| `0x05` | `VM_MAP` | IOW |
| `0x06` | `VM_UNMAP` | IOW |
| `0x07` | `CREATE_CONTEXT` | IOWR |
| `0x08` | `DESTROY_CONTEXT` | IOW |
| `0x09` | `CREATE_FREE_LIST` | IOWR |
| `0x0a` | `DESTROY_FREE_LIST` | IOW |
| `0x0b` | `CREATE_HWRT_DATASET` | IOWR |
| `0x0c` | `DESTROY_HWRT_DATASET` | IOW |
| `0x0d` | `SUBMIT_JOBS` | IOW |

The kernel DRM dispatcher and PVR payload helpers enforce layout, padding and copy bounds.
Handles belong to the opening file.
Unknown flags, overflowing or out-of-range addresses, malformed strides and invalid streams are rejected.

`DEV_QUERY` supports GPU_INFO, RUNTIME_INFO, QUIRKS, ENHANCEMENTS, HEAP_INFO and STATIC_DATA_AREAS.
A null pointer with size zero asks for the required size.
Capabilities and quirks come from the firmware's device-info block, not a driver-specific GPU table.
Mesa's host `drm-shim` with `PVR_SHIM_DEVICE_BVNC=36.29.52.182` can provide an independent comparison;
its availability is not evidence that a comparison has been executed.

`ObjArray` describes stride, count and array pointer.
Short elements are zero-extended; unknown tails of longer elements must be zero.
This rule has one implementation, `uapi::obj_array`, shared by queries, target creation and job/sync arrays.

`GET_BO_MMAP_OFFSET` returns an offset used with `mmap(renderD128, ...)`.
Firmware-protected BOs cannot be CPU-mapped.
Backing is allocated eagerly and remains alive through GPU mappings and CPU VMAs.

## Supported behavior and deliberate differences

### Objects and mappings

- Query containers and element strides are capped at 4096 bytes; array counts and total sizes have separate limits.
- Valued feature parameters are consumed in strictly increasing feature-bit order.
  Unknown feature/quirk bits are retained as unknown and do not imply support.
- Runtime calculations clamp a zero phantom count to one and use saturating subtraction where firmware limits can underflow.
- BOs require contiguous physical backing, unlike Linux shmem; large or sparse allocations may fail.
- User BO mappings use write-combining memory.
  `SUBMIT_JOBS` does not provide a complete BO list from which to infer cache cleaning, and direct user polling has no ioctl sync point.
- CPU-inaccessible BOs are refused by `GET_BO_MMAP_OFFSET`, earlier than Linux's mmap check.
  PM_FW_PROTECT is incompatible with CPU access.
- `VM_MAP` refuses overlapping live ranges rather than splitting and replacing them.
  `VM_UNMAP` requires an exact address/length match.
  An internal duplicate-PTE error currently maps to EINVAL.
- Rogue page tables require 4 KiB alignment and 40-bit addresses.
  PTE writes include DMA synchronization and invalidation bookkeeping; the coherent bit stays clear.
  Removed BO and table backing remains owned until acknowledged invalidation or a confirmed full stop.
  Invalidation is serialized across submitting threads; failed delivery retains both flags and backing.
- Imported dma-bufs retain exporter ownership but do not automatically wait on reservation fences.
  Clients must use explicit synchronization.

### Firmware and ownership

- Firmware objects use `DmaStream` and `FwHeap`, separately from user GEM.
  MIPS is supported; META and RISC-V firmware processors are excluded.
  Unknown, mixed, overflowing or out-of-range layouts return errors.
- MIPS tables are contiguous; map and unmap share one page-range calculation.
  Compile-time assertions fix host and firmware page size to 4 KiB.
- Heap ranges, PTEs and DMA backing have distinct release responsibilities.
  Construction guards and mapped flags roll back only allocations and mappings they own.
- Existing successful owners have fixed ranges within permanently allocated, readable/writable page tables.
  Current `DmaStream` write/sync errors are access or range errors, excluded by these invariants;
  cache maintenance does not report a hardware failure.
  Tests inject failure before PTE writes. A future API with write-after-sync failure requires revisiting rollback and Drop.
- Polling words use CPU uncached access; images and page tables use explicit synchronization.
  CPU and MIPS cache attributes are independent.
- OS initialization publishes KCCB/FWCCB addresses before firmware boot.
- Stop attempts every required step, records the first poll error, and asserts full soft reset.
  META-only steps and the RISC-V FWCORE_BOOT register do not apply to MIPS.
- BRN63553's wrapper workaround follows firmware feature/quirk reporting.
  The Linux warning for physical bus widths other than 36 is not reproduced.

### Scheduling and recovery

- Validation covers the complete batch before fence publication and queue insertion.
  Streams reject unknown extensions, invalid flags, lengths and alignment.
- Geometry/fragment pairs keep their dependency and directional ownership without Arc cycles.
  Native dependencies may become UFO waits; foreign dependencies are resolved on the CPU.
  A failed fence remains a dependency and fails its dependent job.
- Firmware UFO values are 32-bit. Accepted work must eventually signal its fence, including failure/recovery paths.
- Dependency cycles are rejected. The job layer currently maps WouldDeadlock to InvalidArgument (EINVAL rather than EDEADLK).
- HIGH context priority is refused with EOPNOTSUPP because caller privilege is not yet passed to PVR.
  The driver does not implement host priority preemption, load balancing or reset-and-replay.
- Failed firmware cleanup transfers ownership to the service queue rather than releasing DMA.
  A lost flag is separate from confirmed quiescence: failed stop keeps cleanup and retired backing quarantined until reboot.
- Timeouts, power references, watchdog sampling and reset requests control operation; they are not optional diagnostic counters.
- Mapped IRQ lines currently lack masking. Reset/suspend handling therefore suppresses deferred processing
  and tolerates late interrupts rather than claiming Linux's `disable_irq()` contract.

### Platform and diagnostics

The platform layer controls domain power, clocks and reset.
The domain has no implemented power-off sequence; suspension still asserts reset and gates the clock.
Attachment validates identity before render-node registration.
A real, nonzero clock rate is required for firmware timers and power management.

Trace decoding uses the generated format table and bounded output under GPU diagnostic gating.
MIPS NMI state is evidence used in boot-failure classification, not an optional timestamp detail.
Synthetic tests do not execute real firmware, MMIO, power or display sequences.
