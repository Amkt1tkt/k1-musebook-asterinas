<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: PowerVR driver contracts and maintenance. -->

# PowerVR architecture and ownership

This component owns GPU rendering and firmware execution.
Display and generic DRM contracts are documented by the [DRM component](../drm/ARCHITECTURE.md).
Each source module identifies its Linux counterpart; [UPSTREAM](UPSTREAM) fixes the revision.

## Execution paths

`platform/k1` discovers the GPU, controls clocks and reset, and binds its interrupt.
After firmware becomes available, attachment initializes `PvrDevice` and registers the render node.
The interrupt top half acknowledges the event and schedules deferred processing.
The service thread performs work that can sleep: firmware cleanup, reset, timeout checks and autosuspend.

The driver implements 14 private ioctls with per-file object handles.
`sched/job` validates a whole batch's streams, handles, dependencies and geometry/fragment pairing
before publishing fences and queueing work.
Native dependencies can become firmware UFO waits; each consumer retains the producer timeline allocation independently of its context.
Foreign fences, including DRM timeline chains, are waited for by the CPU.
Fence callbacks run outside queue locks.
Combined submission takes geometry before fragment queue locks.
Failed recovery marks the device lost; reset does not replay accepted jobs.

| Address space | Consumer | Type and owner |
| --- | --- | --- |
| CPU/DMA physical | Host and bus | `Paddr` / owned `DmaStream` |
| GPU virtual | Rogue MMU | `DeviceAddr`; VM mappings retain BO references |
| Firmware virtual | MIPS MMU | `FwAddr`; firmware heap objects |

The Rogue MMU has a 40-bit, three-level page table; MIPS has a separate table.
A PTE write includes DMA synchronization.
Rogue invalidation flags accumulate until a serialized firmware round trip completes before submission.
Removed BO and table backing is retired until that acknowledgement or a confirmed full stop.
CPU cache cleaning and GPU TLB invalidation are separate operations.

## Modules

| Module | Responsibility |
| --- | --- |
| `device`, `device_info`, `uapi`, `driver` | Identity, capabilities, ABI and input validation |
| `fw/info`, `elf`, `mips`, `startstop` | Container/ELF validation, image processing and processor control |
| `fw/heap`, `abi`, `trace` | Firmware allocations, shared layouts and bounded diagnostics |
| `mm/gem`, `mmu`, `vm`, `vm_mips` | BO backing and GPU/MIPS address spaces |
| `sched/ccb`, `cccb`, `context`, `job`, `queue`, `stream` | Rings, contexts, batching and stream validation |
| `render/free_list`, `hwrt` | Parameter-buffer pages and render targets |
| `power`, `platform`, `service` | Power references, recovery and deferred work |
| `rogue`, `regen` | Committed hardware definitions and their host generator |

## Allocation and rollback

DMA backing, firmware heap ranges and MIPS PTEs have distinct owners.
`FwAllocation` holds an uncommitted object and its original heap;
failure returns the range before releasing DMA, while commit transfers ownership to the final object.

| Owner | Construction and release rule |
| --- | --- |
| `Queue` / `ClientCcb` | Guards own register state, timeline, control and ring allocations; constructed queues enter context rollback immediately |
| `FwMemContext` | Owner exists before initialization; only a successful map sets the mapped flag |
| `FreeList` | One owner covers initialization, mapping and initial growth; weak registration follows success |
| `HwrtData` | Owns SRTC, RAA and firmware allocations with individual mapped flags |
| `HwrtDataset` | Owns common storage; each target owns its own mappings |

`HwrtRef` retains both dataset and target, keeping common storage and free lists alive while work uses them.
Cleanup removes successful mappings, returns heap ranges, and then releases DMA.
CCB initialization has the same retryable rollback contract before the first firmware boot:
`PendingCcb` and `PendingRings` retain all completed allocations and mappings until publication.

Registries use weak references and must not keep objects alive for diagnostics.
Heap and MIPS write locks follow `heap -> MIPS write`; no reverse acquisition is permitted.
A queue or registry lock must not surround object destruction or an external fence callback.
Bottom-half registry borrows and completed job resources transfer their final references to the service worker.
Failed submitted jobs retain GPU-visible owners until a confirmed stop after their actual kick; fence failure alone is insufficient.
The worker also returns final timeline allocations to the heap and MIPS table.
Failed cleanup returns ownership to the service queue; unconfirmed stop on a lost device quarantines it until reboot.
Failure-injection tests compare complete heap ranges and PTEs while unrelated owners stay live.
They cannot prove concurrent firmware fetch timing.

## Platform boundary

K1 uses noncoherent DMA; see the [DMA contract](CONVENTIONS.md#k1-dma-contract).
The CCU parent-clock operation opens the required APBS and MPMU parent gates.
Clock and idle policy changes require hardware validation.
