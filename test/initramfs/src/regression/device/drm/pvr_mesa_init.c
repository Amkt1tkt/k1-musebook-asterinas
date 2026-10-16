// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: probe the kernel-visible subset of Mesa device initialization.

/*
 * Walk the initialisation sequence Mesa's PowerVR Vulkan driver performs, using
 * raw ioctls.
 *
 * Reproduces a subset of the kernel-visible setup used by Mesa 26.1.6:
 * pvr_physical_device.c, pvr_winsys.c, winsys/powervr/pvr_drm.c and
 * pvr_winsys_helper.c. It checks identity, selected runtime values, heap/static
 * data queries and CPU/GPU mappings. It is not a full Vulkan device-creation
 * test, does not query every extension, and cannot localize all Mesa failures.
 *
 * # What it deliberately does not do
 *
 * It does not write real PDS or USC programs into the static data areas -- those
 * come out of Mesa's compiler. It writes a recognisable pattern and reads it
 * back instead, which tests the thing this port can get wrong (the mapping and
 * its cache policy) rather than the thing it cannot (program encoding).
 *
 * Structures and command numbers are transcribed from include/uapi/drm/pvr_drm.h
 * at the revision named in kernel/core/comps/k1-musebook/pvr/UPSTREAM, for the same reasons
 * pvr_bo.c gives.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#include "../../common/test.h"

#define RENDER_DEVICE "/dev/dri/renderD128"
#define PVR_DRIVER_NAME "powervr"

/* BVNC 36.29.52.182, packed as B<<48 | V<<32 | N<<16 | C. */
#define PVR_BVNC_BXE_2_32 0x0024001d003400b6ULL

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

struct drm_get_cap {
	uint64_t capability;
	uint64_t value;
};

struct drm_gem_close {
	uint32_t handle;
	uint32_t pad;
};

/* Reference: <https://github.com/torvalds/linux/blob/fc02acf6ac0ccde0c805c2daa9148683cdd01ba8/include/uapi/drm/pvr_drm.h>. */
struct drm_pvr_obj_array {
	uint32_t stride;
	uint32_t count;
	uint64_t array;
};

struct drm_pvr_ioctl_dev_query_args {
	uint32_t type;
	uint32_t size;
	uint64_t pointer;
};

struct drm_pvr_dev_query_gpu_info {
	uint64_t gpu_id;
	uint32_t num_phantoms;
	uint32_t _padding_c;
};

struct drm_pvr_dev_query_runtime_info {
	uint64_t free_list_min_pages;
	uint64_t free_list_max_pages;
	uint32_t common_store_alloc_region_size;
	uint32_t common_store_partition_space_size;
	uint32_t max_coeffs;
	uint32_t cdm_max_local_mem_size_regs;
};

struct drm_pvr_heap {
	uint64_t base;
	uint64_t size;
	uint32_t flags;
	uint32_t page_size_log2;
};

struct drm_pvr_dev_query_heap_info {
	struct drm_pvr_obj_array heaps;
};

struct drm_pvr_static_data_area {
	uint16_t area_usage;
	uint16_t location_heap_id;
	uint32_t size;
	uint64_t offset;
};

struct drm_pvr_dev_query_static_data_areas {
	struct drm_pvr_obj_array static_data_areas;
};

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
_Static_assert(sizeof(struct drm_pvr_obj_array) == 16, "drm_pvr_obj_array");
_Static_assert(sizeof(struct drm_pvr_ioctl_dev_query_args) == 16,
	       "drm_pvr_ioctl_dev_query_args");
_Static_assert(sizeof(struct drm_pvr_dev_query_gpu_info) == 16,
	       "drm_pvr_dev_query_gpu_info");
_Static_assert(sizeof(struct drm_pvr_dev_query_runtime_info) == 32,
	       "drm_pvr_dev_query_runtime_info");
_Static_assert(sizeof(struct drm_pvr_heap) == 24, "drm_pvr_heap");
_Static_assert(sizeof(struct drm_pvr_dev_query_heap_info) == 16,
	       "drm_pvr_dev_query_heap_info");
_Static_assert(sizeof(struct drm_pvr_static_data_area) == 16,
	       "drm_pvr_static_data_area");
_Static_assert(sizeof(struct drm_pvr_dev_query_static_data_areas) == 16,
	       "drm_pvr_dev_query_static_data_areas");
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
#define DRM_IOCTL_GET_CAP _IOWR(DRM_IOCTL_BASE, 0x0c, struct drm_get_cap)
#define DRM_IOCTL_GEM_CLOSE _IOW(DRM_IOCTL_BASE, 0x09, struct drm_gem_close)

#define DRM_IOCTL_PVR_DEV_QUERY                        \
	_IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x00, \
	      struct drm_pvr_ioctl_dev_query_args)
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

#define DRM_PVR_DEV_QUERY_GPU_INFO_GET 0
#define DRM_PVR_DEV_QUERY_RUNTIME_INFO_GET 1
#define DRM_PVR_DEV_QUERY_HEAP_INFO_GET 4
#define DRM_PVR_DEV_QUERY_STATIC_DATA_AREAS_GET 5

#define DRM_PVR_HEAP_GENERAL 0
#define DRM_PVR_HEAP_PDS_CODE_DATA 1
#define DRM_PVR_HEAP_USC_CODE 2
#define DRM_PVR_HEAP_RGNHDR 3
#define DRM_PVR_HEAP_VIS_TEST 4
#define DRM_PVR_HEAP_TRANSFER_FRAG 5
#define DRM_PVR_HEAP_COUNT 6

#define DRM_PVR_STATIC_DATA_AREA_EOT 0
#define DRM_PVR_STATIC_DATA_AREA_FENCE 1
#define DRM_PVR_STATIC_DATA_AREA_VDM_SYNC 2
#define DRM_PVR_STATIC_DATA_AREA_YUV_CSC 3

#define DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS (1ULL << 2)

#define DRM_CAP_SYNCOBJ 0x13
#define DRM_CAP_SYNCOBJ_TIMELINE 0x14

static int render_fd = -1;
static int is_powervr;

/* Filled in by the heap and static-data steps, used by the mapping step. */
static struct drm_pvr_heap heaps[DRM_PVR_HEAP_COUNT];
static uint64_t carveout[DRM_PVR_HEAP_COUNT];

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

	/*
	 * Mesa's own first act on the render node: pvr_winsys_create() calls
	 * drmGetVersion() and refuses anything whose name is not "powervr"
	 * (pvr_winsys.c:98). A wrong name here is not a skip for Mesa -- it is
	 * VK_ERROR_INCOMPATIBLE_DRIVER.
	 */
	CHECK(ioctl(render_fd, DRM_IOCTL_VERSION, &version));

	is_powervr = strcmp(name, PVR_DRIVER_NAME) == 0;
	if (!is_powervr)
		fprintf(stderr,
			"%s is a `%s` device, not `%s`; PowerVR tests will be skipped\n",
			RENDER_DEVICE, name, PVR_DRIVER_NAME);
}
END_SETUP()

static int dev_query(uint32_t type, void *out, uint32_t size)
{
	struct drm_pvr_ioctl_dev_query_args args = {
		.type = type,
		.size = size,
		.pointer = (uint64_t)(uintptr_t)out,
	};

	return ioctl(render_fd, DRM_IOCTL_PVR_DEV_QUERY, &args);
}

/*
 * vk_drm_syncobj_get_type() decides the fence backend from these two
 * capabilities on the *render* node. Without them Mesa has no sync object type
 * and device creation fails before any PowerVR ioctl is issued.
 */
FN_TEST(the_render_node_reports_the_syncobj_capabilities)
{
	struct drm_get_cap cap;

	SKIP_TEST_IF(!is_powervr);

	cap.capability = DRM_CAP_SYNCOBJ;
	cap.value = 0;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_GET_CAP, &cap));
	TEST_RES(cap.value, _ret != 0);

	cap.capability = DRM_CAP_SYNCOBJ_TIMELINE;
	cap.value = 0;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_GET_CAP, &cap));
	TEST_RES(cap.value, _ret != 0);
}
END_TEST()

/*
 * the query that tells Mesa which GPU this is. `gpu_id` picks the
 * device-info table (`bxe-2-32.h`); a value Mesa does not know is
 * VK_ERROR_INCOMPATIBLE_DRIVER with "Unsupported BVNC". `num_phantoms` goes
 * straight into shared-register arithmetic.
 */
FN_TEST(gpu_info_names_this_gpu)
{
	struct drm_pvr_dev_query_gpu_info gpu = { 0 };

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(dev_query(DRM_PVR_DEV_QUERY_GPU_INFO_GET, &gpu, sizeof(gpu)));
	/* Printed unconditionally: on a board this is the number to quote. */
	fprintf(stderr, "  gpu_id = 0x%016llx, num_phantoms = %u\n",
		(unsigned long long)gpu.gpu_id, gpu.num_phantoms);
	TEST_RES(gpu.gpu_id, _ret == PVR_BVNC_BXE_2_32);
	/* One cluster, so ceil(1 / 4) == 1. */
	TEST_RES(gpu.num_phantoms, _ret == 1);
}
END_TEST()

/*
 * the six numbers Mesa sizes allocations from. Being wrong here raises
 * no error anywhere -- it renders incorrectly -- so the values are asserted
 * against what tools/fwinfo.py computed from this board's firmware image
 * (the fixed firmware version documented by the PVR UPSTREAM file). The same numbers are pinned by a ktest;
 * this is the end-to-end confirmation that the ioctl delivers them.
 */
FN_TEST(runtime_info_matches_the_firmware_image)
{
	struct drm_pvr_dev_query_runtime_info rt = { 0 };

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(
		dev_query(DRM_PVR_DEV_QUERY_RUNTIME_INFO_GET, &rt, sizeof(rt)));
	TEST_RES(rt.free_list_min_pages, _ret == 25);
	TEST_RES(rt.free_list_max_pages, _ret == 524288);
	TEST_RES(rt.common_store_partition_space_size, _ret == 4096);
	TEST_RES(rt.common_store_alloc_region_size, _ret == 8192);
	TEST_RES(rt.max_coeffs, _ret == 5120);
	TEST_RES(rt.cdm_max_local_mem_size_regs, _ret == 4096);
}
END_TEST()

/*
 * the heap layout, fetched the way Mesa fetches it -- a first call with
 * a null array to learn the count and stride, then a second with the array.
 *
 * The assertions are Mesa's own requirements, because it indexes the returned
 * array by heap id: it writes into `winsys_heaps[i]` for i < count, so a count
 * above DRM_PVR_HEAP_COUNT would walk off the end; every heap except RGNHDR must
 * be present (`size != 0`) or device creation fails with "Required heap not
 * present"; and the page size must equal the host page size, which Mesa asserts
 * (pvr_drm.c:613) and therefore does not check in a release build.
 */
FN_TEST(heap_info_follows_the_two_pass_protocol_and_matches_what_mesa_requires)
{
	struct drm_pvr_dev_query_heap_info query = { 0 };
	long page_size = sysconf(_SC_PAGESIZE);

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(dev_query(DRM_PVR_DEV_QUERY_HEAP_INFO_GET, &query,
			    sizeof(query)));
	TEST_RES(query.heaps.count, _ret == DRM_PVR_HEAP_COUNT);
	TEST_RES(query.heaps.stride, _ret == sizeof(struct drm_pvr_heap));
	SKIP_TEST_IF(query.heaps.count != DRM_PVR_HEAP_COUNT ||
		     query.heaps.stride != sizeof(struct drm_pvr_heap));

	memset(heaps, 0, sizeof(heaps));
	query.heaps.array = (uint64_t)(uintptr_t)heaps;
	TEST_SUCC(dev_query(DRM_PVR_DEV_QUERY_HEAP_INFO_GET, &query,
			    sizeof(query)));

	for (int i = 0; i < DRM_PVR_HEAP_COUNT; i++) {
		/*
		 * RGNHDR exists only on a GPU with BRN63142, which this one is
		 * not. Mesa reads it as absent and falls back to the general
		 * heap, so a zero size is correct rather than a failure.
		 */
		if (i == DRM_PVR_HEAP_RGNHDR)
			continue;

		TEST_RES(heaps[i].size, _ret != 0);
		TEST_RES(heaps[i].base, _ret != 0);
		TEST_RES(heaps[i].page_size_log2,
			 _ret < 63 && (1ULL << _ret) == (uint64_t)page_size);
		TEST_RES(heaps[i].flags, _ret == 0);
	}
}
END_TEST()

/*
 * the static data areas, again two-pass. Mesa turns these into a
 * carveout at the bottom of each heap that its own allocator must not hand out
 * (pvr_drm.c:570-621), so what matters is that every area names a heap that
 * exists and that the per-heap totals fit inside it -- Mesa computes
 * `heap.size - total` as the usable size and would otherwise underflow.
 */
FN_TEST(static_data_areas_fit_inside_the_heaps_they_name)
{
	struct drm_pvr_dev_query_static_data_areas query = { 0 };
	struct drm_pvr_static_data_area areas[16];
	uint32_t count;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(dev_query(DRM_PVR_DEV_QUERY_STATIC_DATA_AREAS_GET, &query,
			    sizeof(query)));
	TEST_RES(query.static_data_areas.count,
		 _ret > 0 &&
			 _ret <= (uint32_t)(sizeof(areas) / sizeof(*areas)));
	TEST_RES(query.static_data_areas.stride,
		 _ret == sizeof(struct drm_pvr_static_data_area));

	SKIP_TEST_IF(query.static_data_areas.count == 0 ||
		     query.static_data_areas.count >
			     sizeof(areas) / sizeof(*areas) ||
		     query.static_data_areas.stride != sizeof(*areas));
	count = query.static_data_areas.count;
	memset(areas, 0, sizeof(areas));
	query.static_data_areas.array = (uint64_t)(uintptr_t)areas;
	TEST_SUCC(dev_query(DRM_PVR_DEV_QUERY_STATIC_DATA_AREAS_GET, &query,
			    sizeof(query)));

	memset(carveout, 0, sizeof(carveout));
	for (uint32_t i = 0; i < count; i++) {
		/*
		 * Mesa skips an unknown heap id rather than failing, so this is
		 * stricter than Mesa on purpose: an area in a heap that does not
		 * exist is a driver bug that Mesa would silently tolerate and
		 * then be unable to fill.
		 */
		TEST_RES(areas[i].location_heap_id, _ret < DRM_PVR_HEAP_COUNT);
		TEST_RES(areas[i].area_usage,
			 _ret <= DRM_PVR_STATIC_DATA_AREA_YUV_CSC);
		if (areas[i].location_heap_id >= DRM_PVR_HEAP_COUNT)
			continue; /* Already failed above; do not index outside the array. */
		carveout[areas[i].location_heap_id] += areas[i].size;
	}

	for (int i = 0; i < DRM_PVR_HEAP_COUNT; i++)
		TEST_RES(carveout[i], _ret <= heaps[i].size);

	/*
	 * The three heaps Mesa fills at init must have something to fill. If
	 * these were all zero the next test would map nothing and still pass.
	 */
	TEST_RES(carveout[DRM_PVR_HEAP_GENERAL], _ret != 0);
	TEST_RES(carveout[DRM_PVR_HEAP_PDS_CODE_DATA], _ret != 0);
	TEST_RES(carveout[DRM_PVR_HEAP_USC_CODE], _ret != 0);
}
END_TEST()

/*
 * Mesa first makes the GPU and the CPU look at
 * the same memory:
 *
 *   CREATE_VM_CONTEXT
 *   for each of general, pds, usc:
 *       CREATE_BO(carveout size, ALLOW_CPU_USERSPACE_ACCESS)
 *       VM_MAP at the heap's base address   <- a fixed address, not one we pick
 *       GET_BO_MMAP_OFFSET + mmap + write + read back
 *       VM_UNMAP, GEM_CLOSE
 *
 * The address is the interesting part: Mesa maps its carveout at exactly
 * `heap.base`, so this is also the test that the driver accepts the lowest
 * address in each heap rather than only addresses in the middle.
 *
 * The read-back is not decoration. These mappings are write-combining, which is
 * what upstream does and what a non-coherent SoC requires; if `svpbmt` were
 * missing the kernel would silently map them writeback instead, and a CPU write
 * the GPU cannot see is invisible from here -- but a mapping that fails
 * outright, or comes back as zeroes, is not.
 */
FN_TEST(mesa_maps_its_static_data_carveout_at_each_heap_base)
{
	struct drm_pvr_ioctl_create_vm_context_args vm = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy_vm = { 0 };
	const int filled[3] = { DRM_PVR_HEAP_GENERAL,
				DRM_PVR_HEAP_PDS_CODE_DATA,
				DRM_PVR_HEAP_USC_CODE };
	long page_size = sysconf(_SC_PAGESIZE);

	SKIP_TEST_IF(!is_powervr);
	/*
	 * Both come from the two queries above. Skipping rather than failing
	 * again keeps one broken query from being reported as three.
	 */
	SKIP_TEST_IF(heaps[DRM_PVR_HEAP_GENERAL].size == 0);
	SKIP_TEST_IF(carveout[DRM_PVR_HEAP_GENERAL] == 0);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &vm));
	TEST_RES(vm.handle, _ret != 0);

	for (int n = 0; n < 3; n++) {
		int heap = filled[n];
		/* Mesa rounds the carveout up to the heap's page size. */
		uint64_t size = (carveout[heap] + page_size - 1) &
				~((uint64_t)page_size - 1);
		struct drm_pvr_ioctl_create_bo_args bo = {
			.size = size,
			.flags = DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
		};
		struct drm_pvr_ioctl_get_bo_mmap_offset_args mmap_offset = { 0 };
		struct drm_pvr_ioctl_vm_map_args map = { 0 };
		struct drm_pvr_ioctl_vm_unmap_args unmap = { 0 };
		struct drm_gem_close close_args = { 0 };
		unsigned char *cpu;
		int ok = 1;

		TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_BO, &bo));
		TEST_RES(bo.handle, _ret != 0);

		map.vm_context_handle = vm.handle;
		map.flags = 0;
		map.device_addr = heaps[heap].base;
		map.handle = bo.handle;
		map.offset = 0;
		map.size = size;
		TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

		mmap_offset.handle = bo.handle;
		TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET,
				&mmap_offset));

		cpu = TEST_RES(mmap(NULL, size, PROT_READ | PROT_WRITE,
				    MAP_SHARED, render_fd, mmap_offset.offset),
			       _ret != MAP_FAILED);
		/* Already counted as a failure; carrying on would segfault. */
		if (cpu == MAP_FAILED)
			break;

		/*
		 * A pattern rather than a real PDS program: this proves the
		 * mapping is readable and writable and that the bytes survive,
		 * which is all this layer can prove. Whether the *GPU* sees them
		 * needs a job that reads them, which is outside this test.
		 */
		for (uint64_t i = 0; i < size; i++)
			cpu[i] = (unsigned char)(i * 7 + heap);
		for (uint64_t i = 0; i < size; i++) {
			if (cpu[i] != (unsigned char)(i * 7 + heap)) {
				ok = 0;
				break;
			}
		}
		TEST_RES(ok, _ret);

		TEST_SUCC(munmap(cpu, size));

		unmap.vm_context_handle = vm.handle;
		unmap.device_addr = heaps[heap].base;
		unmap.size = size;
		TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap));

		close_args.handle = bo.handle;
		TEST_SUCC(ioctl(render_fd, DRM_IOCTL_GEM_CLOSE, &close_args));
	}

	destroy_vm.handle = vm.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT,
			&destroy_vm));
}
END_TEST()

/*
 * Not what Mesa does, but worth pinning while we are here.
 *
 * Mesa's comment at pvr_winsys_helper.c:178 ("this won't destroy bo as its being
 * used by VMA") reads like it closes the GEM handle while the mapping is live
 * and relies on the kernel to keep the object alive. It does not: vma_map takes
 * a userspace reference (pvr_drm_bo.c:497), so GEM_CLOSE comes after VM_UNMAP.
 *
 * The kernel property is real anyway -- a mapping holds a strong reference to the
 * object -- and a future Mesa could rely on it, so this asserts it directly:
 * close the handle first, then unmap, and expect both to succeed.
 */
FN_TEST(a_mapping_outlives_the_handle_it_was_made_from)
{
	struct drm_pvr_ioctl_create_vm_context_args vm = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy_vm = { 0 };
	struct drm_pvr_ioctl_create_bo_args bo = { 0 };
	struct drm_pvr_ioctl_vm_map_args map = { 0 };
	struct drm_pvr_ioctl_vm_unmap_args unmap = { 0 };
	struct drm_gem_close close_args = { 0 };
	long page_size = sysconf(_SC_PAGESIZE);

	SKIP_TEST_IF(!is_powervr);
	SKIP_TEST_IF(heaps[DRM_PVR_HEAP_GENERAL].size == 0);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &vm));

	bo.size = (uint64_t)page_size;
	bo.flags = DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_BO, &bo));

	map.vm_context_handle = vm.handle;
	map.device_addr = heaps[DRM_PVR_HEAP_GENERAL].base;
	map.handle = bo.handle;
	map.size = bo.size;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

	close_args.handle = bo.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_GEM_CLOSE, &close_args));

	/* The handle is gone; the mapping is not, and naming it still works. */
	unmap.vm_context_handle = vm.handle;
	unmap.device_addr = heaps[DRM_PVR_HEAP_GENERAL].base;
	unmap.size = bo.size;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap));

	destroy_vm.handle = vm.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT,
			&destroy_vm));
}
END_TEST()
