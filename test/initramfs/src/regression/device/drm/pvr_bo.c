// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: PowerVR buffer and GPU-VM ABI regression.

/*
 * Buffer objects and GPU address spaces on the PowerVR render node.
 *
 * Exercises CREATE_BO, GET_BO_MMAP_OFFSET,
 * mmap, write, read back — plus the address-space half, CREATE_VM_CONTEXT,
 * VM_MAP, VM_UNMAP and DESTROY_VM_CONTEXT. That round trip is the shortest path
 * that exercises every layer at once: the driver-private ioctl decoder, the GEM
 * handle table, the fake mmap offset space, the demand-paged CPU mapping, and
 * the three-level GPU page table underneath VM_MAP.
 *
 * Structures and command numbers are transcribed from include/uapi/drm/pvr_drm.h
 * rather than included from it, for the reasons drm_smoke.c gives: raw ioctls keep this probe independent of libdrm while separately
 * transcribed layouts check the userspace boundary.
 *
 * Every test is skipped unless /dev/dri/renderD128 is a `powervr` device. On
 * QEMU that node belongs to the software driver, which has no GPU behind it; the
 * checks here need the real one.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include "../../common/test.h"

#define RENDER_DEVICE "/dev/dri/renderD128"

/* The driver name Mesa matches on; see kernel/core/comps/k1-musebook/pvr/ABI.md. */
#define PVR_DRIVER_NAME "powervr"

#define PAGE_SIZE 4096

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L139>. */
struct drm_version {
	int version_major;
	int version_minor;
	int version_patchlevel;
	size_t name_len;
	char *name;
	size_t date_len;
	char *date;
	size_t desc_len;
	char *desc;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L780>. */
struct drm_gem_close {
	uint32_t handle;
	uint32_t pad;
};

/* Reference: <https://github.com/torvalds/linux/blob/fc02acf6ac0ccde0c805c2daa9148683cdd01ba8/include/uapi/drm/pvr_drm.h>. */
struct drm_pvr_ioctl_create_bo_args {
	uint64_t size;
	uint32_t handle;
	uint32_t _padding_c;
	uint64_t flags;
};

struct drm_pvr_ioctl_get_bo_mmap_offset_args {
	uint32_t handle;
	uint32_t _padding_4;
	uint64_t offset;
};

struct drm_pvr_ioctl_create_vm_context_args {
	uint32_t handle;
	uint32_t _padding_4;
};

struct drm_pvr_ioctl_destroy_vm_context_args {
	uint32_t handle;
	uint32_t _padding_4;
};

struct drm_pvr_ioctl_vm_map_args {
	uint32_t vm_context_handle;
	uint32_t flags;
	uint64_t device_addr;
	uint32_t handle;
	uint32_t _padding_14;
	uint64_t offset;
	uint64_t size;
};

struct drm_pvr_ioctl_vm_unmap_args {
	uint32_t vm_context_handle;
	uint32_t _padding_4;
	uint64_t device_addr;
	uint64_t size;
};

/*
 * The ioctl encoding includes the argument size; the dispatcher negotiates
 * compatible short/long structures by command number. These sizes come from
 * Mesa 26.1.6's include/drm-uapi/pvr_drm.h, not from the kernel port.
 */
_Static_assert(sizeof(struct drm_pvr_ioctl_create_bo_args) == 24,
	       "drm_pvr_ioctl_create_bo_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_get_bo_mmap_offset_args) == 16,
	       "drm_pvr_ioctl_get_bo_mmap_offset_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_create_vm_context_args) == 8,
	       "drm_pvr_ioctl_create_vm_context_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_destroy_vm_context_args) == 8,
	       "drm_pvr_ioctl_destroy_vm_context_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_vm_map_args) == 40,
	       "drm_pvr_ioctl_vm_map_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_vm_unmap_args) == 24,
	       "drm_pvr_ioctl_vm_unmap_args");

#define DRM_IOCTL_BASE 'd'
#define DRM_COMMAND_BASE 0x40

#define DRM_IOCTL_VERSION _IOWR(DRM_IOCTL_BASE, 0x00, struct drm_version)
#define DRM_IOCTL_GEM_CLOSE _IOW(DRM_IOCTL_BASE, 0x09, struct drm_gem_close)

#define DRM_IOCTL_PVR_CREATE_BO                        \
	_IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x01, \
	      struct drm_pvr_ioctl_create_bo_args)
#define DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET               \
	_IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x02, \
	      struct drm_pvr_ioctl_get_bo_mmap_offset_args)
#define DRM_IOCTL_PVR_CREATE_VM_CONTEXT                \
	_IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x03, \
	      struct drm_pvr_ioctl_create_vm_context_args)
#define DRM_IOCTL_PVR_DESTROY_VM_CONTEXT              \
	_IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x04, \
	     struct drm_pvr_ioctl_destroy_vm_context_args)
#define DRM_IOCTL_PVR_VM_MAP                          \
	_IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x05, \
	     struct drm_pvr_ioctl_vm_map_args)
#define DRM_IOCTL_PVR_VM_UNMAP                        \
	_IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x06, \
	     struct drm_pvr_ioctl_vm_unmap_args)

/* Reference: <https://github.com/torvalds/linux/blob/fc02acf6ac0ccde0c805c2daa9148683cdd01ba8/include/uapi/drm/pvr_drm.h>. */
#define DRM_PVR_BO_BYPASS_DEVICE_CACHE (1u << 0)
#define DRM_PVR_BO_PM_FW_PROTECT (1u << 1)
#define DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS (1u << 2)

/*
 * The base of the general heap, from pvr_rogue_heap_config.h. Not a value the
 * driver is free to choose — the firmware and Mesa both assume it — and the same
 * number DRM_PVR_DEV_QUERY_HEAP_INFO_GET reports.
 */
#define ROGUE_GENERAL_HEAP_BASE 0x8000000000ull

/* Big enough to span several pages and to cross nothing interesting. */
#define BO_PAGES 4
#define BO_SIZE ((uint64_t)BO_PAGES * PAGE_SIZE)

static int render_fd = -1;
static int is_powervr;

/* The byte a given page should hold. A function of the page index, so that a
 * mapping which resolved the same page twice, or the wrong one, reads back a
 * wrong value instead of looking fine. */
static unsigned char pattern_of(size_t page)
{
	return (unsigned char)(0xa0 + (page % 91));
}

static int create_bo(uint64_t size, uint64_t flags, uint32_t *handle)
{
	struct drm_pvr_ioctl_create_bo_args args = { .size = size,
						     .flags = flags };

	if (ioctl(render_fd, DRM_IOCTL_PVR_CREATE_BO, &args) < 0)
		return -1;

	*handle = args.handle;
	return 0;
}

static int mmap_offset_of(uint32_t handle, uint64_t *offset)
{
	struct drm_pvr_ioctl_get_bo_mmap_offset_args args = { .handle =
								      handle };

	if (ioctl(render_fd, DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET, &args) < 0)
		return -1;

	*offset = args.offset;
	return 0;
}

static int close_handle(uint32_t handle)
{
	struct drm_gem_close close_arg = { .handle = handle };

	return ioctl(render_fd, DRM_IOCTL_GEM_CLOSE, &close_arg);
}

FN_SETUP(open_render_node)
{
	char name[64] = { 0 };
	struct drm_version version = { .name_len = sizeof(name) - 1,
				       .name = name };

	render_fd = open(RENDER_DEVICE, O_RDWR);
	if (render_fd < 0) {
		if (errno == ENOENT || errno == ENODEV || errno == ENXIO) {
			fprintf(stderr, "SKIP: %s is absent (%s)\n",
				RENDER_DEVICE, strerror(errno));
			return;
		}
		perror("open " RENDER_DEVICE);
		exit(EXIT_FAILURE);
	}

	CHECK(ioctl(render_fd, DRM_IOCTL_VERSION, &version));

	is_powervr = strcmp(name, PVR_DRIVER_NAME) == 0;
	if (!is_powervr)
		fprintf(stderr,
			"%s is a `%s` device, not `%s`; PowerVR tests will be skipped\n",
			RENDER_DEVICE, name, PVR_DRIVER_NAME);
}
END_SETUP()

/*
 * Buffer round trip: allocate, get an offset, map it, write
 * through the mapping, read it back.
 *
 * Reading back what was written is not a formality on this SoC. The mapping is
 * write-combining because the K1 has no hardware DMA coherency, and the pages
 * are handed to the process one at a time by the fault handler; a buffer that
 * resolved the wrong frame, or the same frame more than once, fails here.
 */
FN_TEST(create_map_write_and_read_back)
{
	uint32_t handle = 0;
	uint64_t offset = 0;
	unsigned char *mem;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(create_bo(BO_SIZE, DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
			    &handle));
	// Handle zero is never valid: user space uses it as "no buffer".
	TEST_RES(handle, _ret != 0);

	TEST_SUCC(mmap_offset_of(handle, &offset));
	// The offset space starts at 4 GiB, as Linux's does.
	TEST_RES(offset, _ret >= (1ull << 32));

	// Asking twice must give the same answer, or a client's second mapping
	// would be of somebody else's buffer.
	uint64_t again = 0;
	TEST_SUCC(mmap_offset_of(handle, &again));
	TEST_RES(again, _ret == offset);

	mem = TEST_RES(mmap(NULL, BO_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED,
			    render_fd, (off_t)offset),
		       _ret != MAP_FAILED);
	if (mem == MAP_FAILED)
		goto out;

	// A fresh buffer is zeroed: these pages come straight from the frame
	// allocator into a user address space.
	int dirty = 0;
	for (size_t page = 0; page < BO_PAGES; ++page) {
		const volatile unsigned char *p = mem + page * PAGE_SIZE;

		if (p[0] != 0 || p[PAGE_SIZE - 1] != 0)
			dirty++;
	}
	TEST_RES(dirty, _ret == 0);

	for (size_t page = 0; page < BO_PAGES; ++page) {
		volatile unsigned char *p = mem + page * PAGE_SIZE;

		p[0] = pattern_of(page);
		p[PAGE_SIZE / 2] = pattern_of(page);
		p[PAGE_SIZE - 1] = pattern_of(page);
	}

	int mismatches = 0;
	for (size_t page = 0; page < BO_PAGES; ++page) {
		const volatile unsigned char *p = mem + page * PAGE_SIZE;
		unsigned char expected = pattern_of(page);

		if (p[0] == expected && p[PAGE_SIZE / 2] == expected &&
		    p[PAGE_SIZE - 1] == expected)
			continue;

		fprintf(stderr, "page %zu does not read back\n", page);
		mismatches++;
	}
	TEST_RES(mismatches, _ret == 0);

	TEST_SUCC(munmap(mem, BO_SIZE));

out:
	TEST_SUCC(close_handle(handle));
}
END_TEST()

/*
 * CREATE_BO's rejections. Every one of them is upstream's, and each protects
 * something: a partial page would map memory the caller did not ask for, an
 * unknown flag would be silently ignored by a driver that later gives it a
 * meaning, and a firmware-protected buffer must never also be CPU mappable.
 */
FN_TEST(create_bo_rejects_bad_arguments)
{
	struct drm_pvr_ioctl_create_bo_args args;
	uint32_t handle = 0;

	SKIP_TEST_IF(!is_powervr);

	TEST_ERRNO(create_bo(0, 0, &handle), EINVAL);
	TEST_ERRNO(create_bo(PAGE_SIZE - 1, 0, &handle), EINVAL);
	TEST_ERRNO(create_bo(PAGE_SIZE + 1, 0, &handle), EINVAL);

	// Bit 3 is undefined, and bit 63 is one of the kernel-only flags
	// upstream reserves at the top of the word.
	TEST_ERRNO(create_bo(PAGE_SIZE, 1u << 3, &handle), EINVAL);
	TEST_ERRNO(create_bo(PAGE_SIZE, 1ull << 63, &handle), EINVAL);

	TEST_ERRNO(create_bo(PAGE_SIZE,
			     DRM_PVR_BO_PM_FW_PROTECT |
				     DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
			     &handle),
		   EINVAL);

	// Padding must be zero.
	memset(&args, 0, sizeof(args));
	args.size = PAGE_SIZE;
	args._padding_c = 1;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_BO, &args), EINVAL);
}
END_TEST()

/*
 * A buffer the client did not ask for CPU access to must not be mappable, and a
 * firmware-protected one must not be either. Upstream refuses the mmap itself;
 * this port refuses to hand out the offset, because it has no per-object mmap
 * hook to refuse in. See kernel/core/comps/k1-musebook/pvr/ABI.md.
 */
FN_TEST(only_cpu_accessible_buffers_get_an_offset)
{
	uint32_t plain = 0, protected_bo = 0;
	uint64_t offset = 0;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(create_bo(PAGE_SIZE, 0, &plain));
	TEST_ERRNO(mmap_offset_of(plain, &offset), EINVAL);

	TEST_SUCC(
		create_bo(PAGE_SIZE, DRM_PVR_BO_PM_FW_PROTECT, &protected_bo));
	TEST_ERRNO(mmap_offset_of(protected_bo, &offset), EINVAL);

	// A handle that was never issued is ENOENT, not EINVAL: user space
	// branches on the difference.
	TEST_ERRNO(mmap_offset_of(0xdead, &offset), ENOENT);

	TEST_SUCC(close_handle(plain));
	TEST_SUCC(close_handle(protected_bo));
}
END_TEST()

/*
 * The GPU address space half. VM_MAP writes the three-level page table; there is
 * no way to observe the entries from user space, so what is checked here is the
 * contract around them: the mapping is accepted where it should be, refused
 * where it should be, and can be taken down again.
 */
FN_TEST(vm_context_map_and_unmap)
{
	struct drm_pvr_ioctl_create_vm_context_args create = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy = { 0 };
	uint32_t handle = 0;
	uint32_t vm = 0;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &create));
	vm = create.handle;
	TEST_RES(vm, _ret != 0);

	TEST_SUCC(create_bo(BO_SIZE, DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
			    &handle));

	struct drm_pvr_ioctl_vm_map_args map = {
		.vm_context_handle = vm,
		.device_addr = ROGUE_GENERAL_HEAP_BASE,
		.handle = handle,
		.offset = 0,
		.size = BO_SIZE,
	};
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

	// Mapping the same range twice must be refused rather than silently
	// replacing the first mapping.
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map), EINVAL);

	// So must a range that overlaps it only partly.
	struct drm_pvr_ioctl_vm_map_args overlap = map;
	overlap.device_addr = ROGUE_GENERAL_HEAP_BASE + PAGE_SIZE;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &overlap), EINVAL);

	// An address outside every heap, a misaligned one, and a length that
	// runs off the end of the buffer object.
	struct drm_pvr_ioctl_vm_map_args bad = map;
	bad.device_addr = 0;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &bad), EINVAL);
	bad.device_addr = ROGUE_GENERAL_HEAP_BASE + 0x10000 + 8;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &bad), EINVAL);
	bad.device_addr = ROGUE_GENERAL_HEAP_BASE + 0x100000;
	bad.size = BO_SIZE + PAGE_SIZE;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &bad), EINVAL);
	// And a flag nobody has defined yet.
	bad = map;
	bad.device_addr = ROGUE_GENERAL_HEAP_BASE + 0x100000;
	bad.flags = 1;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &bad), EINVAL);

	// An unknown buffer object is ENOENT; an unknown VM context is EINVAL.
	bad = map;
	bad.device_addr = ROGUE_GENERAL_HEAP_BASE + 0x100000;
	bad.handle = 0xdead;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &bad), ENOENT);
	bad = map;
	bad.device_addr = ROGUE_GENERAL_HEAP_BASE + 0x100000;
	bad.vm_context_handle = 0xdead;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &bad), EINVAL);

	// Unmap names a mapping exactly: right address, wrong length is ENOENT.
	struct drm_pvr_ioctl_vm_unmap_args unmap = {
		.vm_context_handle = vm,
		.device_addr = ROGUE_GENERAL_HEAP_BASE,
		.size = BO_SIZE - PAGE_SIZE,
	};
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap), ENOENT);

	unmap.size = BO_SIZE;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap));
	// Twice is ENOENT: there is nothing there any more.
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap), ENOENT);

	// The address is free again once unmapped.
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

	// Destroying the context takes the surviving mapping with it.
	destroy.handle = vm;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT, &destroy));
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT, &destroy),
		   EINVAL);

	TEST_SUCC(close_handle(handle));
}
END_TEST()

/*
 * A buffer stays alive while it is mapped into a GPU address space, even after
 * the client closes its handle. That is what stops user space from pulling the
 * pages out from under the GPU, and it is why closing a handle is not a destroy
 * operation.
 *
 * There is nothing to observe from here beyond "the kernel did not fall over",
 * which is exactly the point: the same sequence against a driver that dropped
 * the pages would leave the page tables naming freed memory.
 */
FN_TEST(a_mapped_buffer_outlives_its_handle)
{
	struct drm_pvr_ioctl_create_vm_context_args create = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy = { 0 };
	uint32_t handle = 0;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &create));
	TEST_SUCC(create_bo(BO_SIZE, DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
			    &handle));

	struct drm_pvr_ioctl_vm_map_args map = {
		.vm_context_handle = create.handle,
		.device_addr = ROGUE_GENERAL_HEAP_BASE,
		.handle = handle,
		.size = BO_SIZE,
	};
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

	TEST_SUCC(close_handle(handle));
	// The handle is gone, so mapping it again fails...
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map), ENOENT);

	// ...but the existing mapping is still there and still removable.
	struct drm_pvr_ioctl_vm_unmap_args unmap = {
		.vm_context_handle = create.handle,
		.device_addr = ROGUE_GENERAL_HEAP_BASE,
		.size = BO_SIZE,
	};
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap));

	destroy.handle = create.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT, &destroy));
}
END_TEST()

/* Padding fields are part of the ABI and must be zero in every argument. */
FN_TEST(padding_must_be_zero)
{
	struct drm_pvr_ioctl_create_vm_context_args create = { ._padding_4 =
								       1 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy = {
		.handle = 1, ._padding_4 = 1
	};
	struct drm_pvr_ioctl_get_bo_mmap_offset_args offset = {
		.handle = 1, ._padding_4 = 1
	};
	struct drm_pvr_ioctl_vm_map_args map = { .vm_context_handle = 1,
						 ._padding_14 = 1 };
	struct drm_pvr_ioctl_vm_unmap_args unmap = { .vm_context_handle = 1,
						     ._padding_4 = 1 };

	SKIP_TEST_IF(!is_powervr);

	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &create),
		   EINVAL);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT, &destroy),
		   EINVAL);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET, &offset),
		   EINVAL);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map), EINVAL);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap), EINVAL);
}
END_TEST()

FN_SETUP(close_render_node)
{
	if (render_fd >= 0)
		CHECK(close(render_fd));
}
END_SETUP()
