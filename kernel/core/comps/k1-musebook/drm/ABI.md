<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: current DRM ABI scope and deliberate limitations. -->

# User-space interface

The crate uses Linux DRM ioctl numbers, structure layouts, and capability bits.
It implements the subset used by COSMIC and Mesa, rather than the complete Linux
DRM subsystem. GPU-private commands and generated firmware layouts are described
in the [PowerVR ABI](../pvr/ABI.md).

| Area | Implementation and boundary |
| --- | --- |
| Devices and permissions | Primary/render nodes, mastership, authentication, privilege, and render-node checks. Advertised capabilities depend on driver features. |
| GEM | Per-file reusable handles, dumb buffers, flink/open, and fake mmap offsets. The kernel validates a handle and complete object range when opening a mapping; its handler retains that object until the final unmap. |
| PRIME | One live GEM object exports to one dma-buf. Repeated or concurrent imports of that dma-buf in one file resolve to one live handle. Foreign drivers may create hardware-specific views; re-export preserves the original buffer identity. |
| KMS | Resources, properties, blobs, framebuffers, legacy requests, and atomic commits. Only the single-plane formats recognized by the FourCC helpers are accepted. Drivers determine modes, scaling, and modifiers. |
| Completion events | A 4 KiB per-file budget is reserved before submission. A fault during user copying preserves both the event and its budget. `AtVblank` defers completion; `Immediate` only indicates driver submission. |
| Synchronization | Binary/timeline sync objects, sync files, eventfds, waits, signal, query, and transfer. A wait accepts at most 1024 handles; a file holds at most 4096 sync objects. |

Enumeration reports the total available count, separately from the number
copied. Each ioctl chooses truncating copy or whole-array copy as its Linux
counterpart requires. Generic input arrays are limited to 1 MiB. Handlers
explicitly reject unsupported flags, nonzero reserved fields, overflow, and
invalid object types.

Legacy requests use atomic validation. The property set is fixed. `NONBLOCK`
does not create a second submission thread. KMS IDs are not reused; dynamic
allocation returns `ENOSPC` when the namespace is exhausted. There is no general
`AUTH_MAGIC` implementation, and unlisted ioctls remain unsupported.

Timeline points are strictly increasing. A point includes unfinished work from
preceding points; completing a later fence early cannot satisfy its timeline
wait. Retiring a completed prefix preserves its query value. Transfer of a
completed point produces a signaled fence. `WAIT_FOR_SUBMIT` in transfer has a
five-second interruptible deadline; callers without a blocking adapter receive
`EOPNOTSUPP` when submission is required. Array signal validates requests before
installation, but is not a multi-object transaction against concurrent ioctls.
`PRIME_FD_TO_HANDLE` rejects its unused flags; some error checks are stricter than
Linux. Per-handler comments identify these differences.

Structure-layout assertions and software-device tests do not establish hardware
register behavior, DMA/cache coherency, mode switching, or fault recovery.

A hardware update retains old scanout owners until independent controller proof
shows that old DMA reads have finished. Flip-event delivery and the three-vblank
fallback do not authorize memory release. At most two unconfirmed framebuffer
replacements are retained per CRTC; further replacements return `EBUSY` before
programming the driver. Geometry-only updates keep the same buffers in the
published state. File closure cancels events without discarding DMA owners.

`ROOT_ONLY` checks the current caller's initial-user-namespace `CAP_SYS_ADMIN`.
`SET_MASTER` and `DROP_MASTER` accept that capability or the original opening
thread of a former master. Passing a descriptor does not transfer this opener
exception. Authentication remains a per-file grant initialized at open.
