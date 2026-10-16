<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: DRM ownership and concurrency contracts. -->

# Ownership and concurrency

The crate implements the DRM object model and ABI. The kernel adapters provide
user-memory, descriptor, credential, and blocking services. Drivers provide
buffer allocation, foreign-buffer import, and display submission. Dependencies
run from PowerVR and framebuffer to DRM, then to OSTD and softirq; DRM must not
depend on a GPU driver.

Each open owns a `DrmFile`, with its GEM and sync-object handle tables and event
budget. Each kernel mapping handler retains one resolved GEM object; its range
cannot cross into a neighboring fake offset. GEM objects and dma-bufs have
independent `Arc` lifetimes. Closing a handle cannot release memory retained by a mapping,
framebuffer, or shared descriptor.

Before a private mapping copies an original DMA-backed page through the kernel
alias, its handler calls `GemObject::prepare_cpu_read` for that object-local page.
The hook is nonblocking and performs cache synchronization without waiting for
GPU jobs. Private copies use writeback cache policy and retain a software PTE
identity through mprotect and fork, so a later COW of a private copy does not
synchronize the original object again. Shared mappings retain their object policy.

The kernel supplies current capabilities and an opaque thread identity for each
ioctl. `ROOT_ONLY` never uses the opener's capability snapshot. Mastership changes
require current initial-user-namespace `CAP_SYS_ADMIN`, or the original opener of
a file that was already master. The identity retains a weak allocation reference,
so numeric task-ID reuse cannot impersonate the opener and no thread/file cycle
is introduced. Missing caller identity never grants the opener exception.

PRIME export checks and publishes identity under a global weak-reference table.
Candidate allocation and destruction occur outside that lock. Foreign import
runs the driver outside the handle-table lock and rechecks identity when
installing the handle. Closing a handle removes its weak import metadata before
that number can be reused.

Legacy KMS requests become property assignments and share the atomic validation
and commit path. A sleepable `commit_lock` serializes commits; software state is
published only after the driver succeeds. Drivers must undo partial hardware
changes before returning an error. Candidate and committed states retain their
mode blobs, independently of the creating file. Fixed CRTC, plane, and connector
tables do not change after registration.

`Vblank` uses a short `LocalIrqDisabled` state lock. Drivers begin a generation
before shadow publication and call `observe_hardware` under their register/IRQ
lock. The observation records a real vblank, the update eligible for a flip
event, and an independently proven scanout-retirement prefix. It performs no
callbacks or resource destruction. The bottom half calls
`dispatch_observed_vblank` without counting the observation twice. An early flip
acknowledgment survives software publication and event parking; a stale bottom
half cannot complete a newer update.

Flip acknowledgment does not authorize DMA release. The K1 adapter preserves the
BSP's flip-event timing while requiring a latch observed in an earlier IRQ
acknowledgment before a later real vblank proves old scanout retirement. Sticky
latch/vsync bits in one acknowledgment cannot establish that ordering. A later
commit preserves the preceding latch generation, so continuous merged status can
acknowledge each new flip and retire older storage without adding an event-frame
wait. This software protocol still requires controller timing validation on K1.

Old atomic states retain framebuffer/GEM owners until retirement proof, including
after event delivery, the three-tick fallback, or file closure. Each CRTC permits
at most two unconfirmed framebuffer replacements and then returns `EBUSY` before
another replacement writes hardware. Geometry-only updates retain their existing
buffer in the new state and need no extra owner. A later retired generation
covers all older transitions. Process-context commit or file cleanup collects
those references and releases them outside the state lock. Interrupt observations
and bottom-half event delivery never run driver-object destructors. Hardware
immediate-event fallback needs the same ownership protection; pure software
drivers require no hardware generation.

`FileEvents`, `FenceState`, and `FenceWaiter` use `BottomHalfDisabled` locks and
support process and bottom-half contexts, not hard interrupts. Notifications,
fence callbacks, waiter wakeups, and retired driver-object destruction run
outside the corresponding object lock.

Dependency edits take `GRAPH` before any fence-state lock. Signalling does not
take `GRAPH`, so insertion must recheck terminal status under the fence lock.
Wait attachments detach through RAII on success, timeout, and interruption.
Unfinished earlier timeline points are included in each point's completion.
Callback registration occurs outside the sync-object lock; publication rechecks
the prefix in case registration raced with reset or submission. A shared
completion and retirement trampoline processes cascades iteratively, including
chains transferred across sync objects. Queue state is locked briefly; callbacks
and final-reference destruction run outside that lock. Fence implementations
must defer any resource destruction that needs a sleepable lock.

Submission waits retain the requested timeline point; an older submitted point
does not satisfy a wait for a future one. Completed timeline prefixes are retired
under the same sync-object lock as reset, and their value remains queryable.

PowerVR firmware and hardware lock ordering are documented in its
[architecture](../pvr/ARCHITECTURE.md). The software protocols do not establish
controller timing or RISC-V hardware memory ordering.
