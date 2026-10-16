// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: cross-device PRIME import and lifetime regression.

/*
 * Buffer sharing between the display controller and the GPU.
 *
 * The two devices on this board are separate DRM drivers with separate nodes:
 * `spacemit` owns the DPU and the panel and has no rendering engine, `powervr`
 * owns the GPU and has no scanout. Every frame a compositor puts on screen
 * therefore crosses between them, and it crosses through PRIME. The tests cover both directions, which
 * drm_smoke does not: that probe imports back into the exporting device.
 *
 * Which direction matters more is not obvious, so both are here:
 *
 *   display -> GPU   is what `cosmic-comp` needs. Its KMS backend opens GBM on
 *                    the *primary* node, allocates there, and asks EGL to
 *                    render into that allocation. The GPU must therefore be
 *                    able to map memory it did not allocate.
 *   GPU -> display   is what an offscreen renderer needs to get a frame on
 *                    screen, and what Mesa's VK_KHR_display WSI does.
 *
 * The failure this file is really written against is silent. The DPU has no
 * MMU: it is handed one physical address and it reads linearly from there. If
 * an import ever produced a copy of the pages rather than the pages, the GPU
 * would render into one buffer while the panel scanned out another, and every
 * ioctl involved would return success. The kernel has a guard for that case
 * (`PvrObject::map_foreign` compares the DMA address against the exporter's
 * physical address); these tests are what make the guard reachable.
 *
 * Structures and command numbers are transcribed from the uapi headers rather
 * than included from libdrm, for the reason drm_smoke.c gives: a test that
 * restates the ABI it checks fails loudly when the kernel disagrees, instead
 * of agreeing with the kernel because both read the same definition.
 *
 * Every test is skipped unless both nodes are present with the expected driver
 * names. On QEMU they are not.
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

#define CARD_DEVICE "/dev/dri/card0"
#define RENDER_DEVICE "/dev/dri/renderD128"

/* The driver names both halves match on; see kernel/core/comps/k1-musebook/pvr/ABI.md. */
#define CARD_DRIVER_NAME "spacemit"
#define RENDER_DRIVER_NAME "powervr"

#define PAGE_SIZE 4096

/*
 * 256x256 at 32bpp: 256 KiB, sixty-four pages. Large enough that the buffer
 * spans multiple GPU PTEs, small enough to be free.
 * Not the panel's 1920x1080, because nothing here scans out and a full-screen
 * allocation would only make a failure more expensive.
 */
#define FB_WIDTH 256u
#define FB_HEIGHT 256u
#define FB_BPP 32u
#define FB_PITCH (FB_WIDTH * (FB_BPP / 8u))
#define FB_SIZE ((uint64_t)FB_PITCH * FB_HEIGHT)

/* fourcc('X', 'R', '2', '4'), the only format the K1 plane advertises besides ARGB. */
#define DRM_FORMAT_XRGB8888 0x34325258u

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

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L1032>. */
struct drm_prime_handle {
	uint32_t handle;
	uint32_t flags;
	int32_t fd;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L967>. */
struct drm_mode_create_dumb {
	uint32_t height;
	uint32_t width;
	uint32_t bpp;
	uint32_t flags;
	uint32_t handle;
	uint32_t pitch;
	uint64_t size;
};

struct drm_mode_map_dumb {
	uint32_t handle;
	uint32_t pad;
	uint64_t offset;
};

struct drm_mode_destroy_dumb {
	uint32_t handle;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L560>. */
struct drm_mode_fb_cmd2 {
	uint32_t fb_id;
	uint32_t width;
	uint32_t height;
	uint32_t pixel_format;
	uint32_t flags;
	uint32_t handles[4];
	uint32_t pitches[4];
	uint32_t offsets[4];
	uint64_t modifier[4];
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
 * compatible sizes by command number. These independent layout assertions
 * come from the uapi headers, not from this port.
 */
_Static_assert(sizeof(struct drm_prime_handle) == 12, "drm_prime_handle");
_Static_assert(sizeof(struct drm_mode_create_dumb) == 32,
	       "drm_mode_create_dumb");
_Static_assert(sizeof(struct drm_mode_map_dumb) == 16, "drm_mode_map_dumb");
_Static_assert(sizeof(struct drm_mode_fb_cmd2) == 104, "drm_mode_fb_cmd2");
_Static_assert(sizeof(struct drm_pvr_ioctl_create_bo_args) == 24,
	       "drm_pvr_ioctl_create_bo_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_vm_map_args) == 40,
	       "drm_pvr_ioctl_vm_map_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_vm_unmap_args) == 24,
	       "drm_pvr_ioctl_vm_unmap_args");

#define DRM_IOCTL_BASE 'd'
#define DRM_COMMAND_BASE 0x40

#define DRM_IOCTL_VERSION _IOWR(DRM_IOCTL_BASE, 0x00, struct drm_version)
#define DRM_IOCTL_GEM_CLOSE _IOW(DRM_IOCTL_BASE, 0x09, struct drm_gem_close)
#define DRM_IOCTL_PRIME_HANDLE_TO_FD \
	_IOWR(DRM_IOCTL_BASE, 0x2d, struct drm_prime_handle)
#define DRM_IOCTL_PRIME_FD_TO_HANDLE \
	_IOWR(DRM_IOCTL_BASE, 0x2e, struct drm_prime_handle)
#define DRM_IOCTL_MODE_ADDFB2 \
	_IOWR(DRM_IOCTL_BASE, 0xB8, struct drm_mode_fb_cmd2)
#define DRM_IOCTL_MODE_RMFB _IOWR(DRM_IOCTL_BASE, 0xAF, unsigned int)
#define DRM_IOCTL_MODE_CREATE_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB2, struct drm_mode_create_dumb)
#define DRM_IOCTL_MODE_MAP_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB3, struct drm_mode_map_dumb)
#define DRM_IOCTL_MODE_DESTROY_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB4, struct drm_mode_destroy_dumb)

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

#define DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS (1u << 2)

/*
 * The base of the general heap, from pvr_rogue_heap_config.h. Not a value the
 * driver is free to choose — the firmware and Mesa both assume it.
 */
#define ROGUE_GENERAL_HEAP_BASE 0x8000000000ull

/* From drm.h: PRIME export wants both, and O_RDWR is what a mappable share needs. */
#define DRM_CLOEXEC O_CLOEXEC
#define DRM_RDWR O_RDWR

static int card_fd = -1;
static int render_fd = -1;
static int both_present;

static int driver_name_is(int fd, const char *expected)
{
	char name[64];
	struct drm_version version;

	memset(&version, 0, sizeof(version));
	memset(name, 0, sizeof(name));
	version.name = name;
	version.name_len = sizeof(name) - 1;
	CHECK(ioctl(fd, DRM_IOCTL_VERSION, &version));
	return strcmp(name, expected) == 0;
}

FN_SETUP(open_both_nodes)
{
	const char *nodes[] = { CARD_DEVICE, RENDER_DEVICE };
	int *fds[] = { &card_fd, &render_fd };
	for (size_t i = 0; i < sizeof(nodes) / sizeof(nodes[0]); ++i) {
		*fds[i] = open(nodes[i], O_RDWR | O_CLOEXEC);
		if (*fds[i] < 0 && errno != ENOENT && errno != ENODEV &&
		    errno != ENXIO) {
			fprintf(stderr, "fatal error: open %s: %s\n", nodes[i],
				strerror(errno));
			exit(EXIT_FAILURE);
		}
	}

	both_present = card_fd >= 0 && render_fd >= 0 &&
		       driver_name_is(card_fd, CARD_DRIVER_NAME) &&
		       driver_name_is(render_fd, RENDER_DRIVER_NAME);

	if (!both_present) {
		fprintf(stderr,
			"prime_cross: skipping, need %s to be '%s' and %s to be '%s'\n",
			CARD_DEVICE, CARD_DRIVER_NAME, RENDER_DEVICE,
			RENDER_DRIVER_NAME);
	}
}
END_SETUP()

static int export_fd(int fd, uint32_t handle, int *out)
{
	struct drm_prime_handle args = { .handle = handle,
					 .flags = DRM_CLOEXEC | DRM_RDWR,
					 .fd = -1 };

	if (ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &args) < 0)
		return -1;
	*out = args.fd;
	return 0;
}

static int import_handle(int fd, int dmabuf, uint32_t *out)
{
	struct drm_prime_handle args = { .fd = dmabuf };

	if (ioctl(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &args) < 0)
		return -1;
	*out = args.handle;
	return 0;
}

static int close_handle(int fd, uint32_t handle)
{
	struct drm_gem_close args = { .handle = handle };

	return ioctl(fd, DRM_IOCTL_GEM_CLOSE, &args);
}

static int create_dumb(uint32_t *handle, uint32_t *pitch, uint64_t *size)
{
	struct drm_mode_create_dumb args = { .width = FB_WIDTH,
					     .height = FB_HEIGHT,
					     .bpp = FB_BPP };

	if (ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &args) < 0)
		return -1;
	*handle = args.handle;
	*pitch = args.pitch;
	*size = args.size;
	return 0;
}

static int destroy_dumb(uint32_t handle)
{
	struct drm_mode_destroy_dumb args = { .handle = handle };

	return ioctl(card_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &args);
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

/*
 * The display half of the hand-off, as `cosmic-comp` does it: allocate on the
 * primary node, then ask the GPU to render into it.
 *
 * VM_MAP is the assertion that matters. It is where the driver walks the
 * imported buffer's frames into the three-level GPU page table, and it is the
 * first place an import without usable frames can be caught.
 */
FN_TEST(the_gpu_maps_a_buffer_the_display_controller_allocated)
{
	uint32_t dumb_handle = 0, pitch = 0, imported = 0, vm_context = 0;
	uint64_t size = 0;
	int dmabuf = -1;
	struct drm_pvr_ioctl_create_vm_context_args create_vm = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy_vm = { 0 };
	struct drm_pvr_ioctl_vm_map_args map = { 0 };
	struct drm_pvr_ioctl_vm_unmap_args unmap = { 0 };

	SKIP_TEST_IF(!both_present);

	TEST_SUCC(create_dumb(&dumb_handle, &pitch, &size));
	TEST_RES(pitch, _ret >= FB_PITCH);
	TEST_RES(size, _ret >= FB_SIZE);

	TEST_SUCC(export_fd(card_fd, dumb_handle, &dmabuf));
	TEST_RES(dmabuf, _ret >= 0);

	TEST_SUCC(import_handle(render_fd, dmabuf, &imported));
	TEST_RES(imported, _ret != 0);

	TEST_SUCC(
		ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &create_vm));
	vm_context = create_vm.handle;

	map.vm_context_handle = vm_context;
	map.device_addr = ROGUE_GENERAL_HEAP_BASE;
	map.handle = imported;
	map.offset = 0;
	map.size = size;
	/*
	 * If this returns EINVAL on hardware, the first suspect is the bounce
	 * buffer guard in PvrObject::map_foreign, which refuses an import the
	 * platform can only reach through a copy. The console carries the
	 * reason; the errno alone cannot distinguish it from a bad argument.
	 */
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

	unmap.vm_context_handle = vm_context;
	unmap.device_addr = ROGUE_GENERAL_HEAP_BASE;
	unmap.size = size;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap));

	destroy_vm.handle = vm_context;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT,
			&destroy_vm));
	TEST_SUCC(close_handle(render_fd, imported));
	TEST_SUCC(close(dmabuf));
	TEST_SUCC(destroy_dumb(dumb_handle));
}
END_TEST()

/*
 * An imported buffer must not gain a second CPU mapping through the importer.
 *
 * This is not a policy preference. The exporter mapped those pages with its own
 * cache attributes, and this SoC has no DMA coherency to reconcile two mappings
 * that disagree — the result is a buffer written through one and read through
 * the other with nothing reporting an error. Upstream refuses for the same
 * reason (`drm_gem_dumb_map_offset` rejects `obj->import_attach`).
 */
FN_TEST(an_imported_buffer_has_no_cpu_mapping_on_the_gpu_side)
{
	uint32_t dumb_handle = 0, pitch = 0, imported = 0;
	uint64_t size = 0;
	int dmabuf = -1;
	struct drm_pvr_ioctl_get_bo_mmap_offset_args offset = { 0 };

	SKIP_TEST_IF(!both_present);

	TEST_SUCC(create_dumb(&dumb_handle, &pitch, &size));
	TEST_SUCC(export_fd(card_fd, dumb_handle, &dmabuf));
	TEST_SUCC(import_handle(render_fd, dmabuf, &imported));

	offset.handle = imported;
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET, &offset),
		   EINVAL);

	TEST_SUCC(close_handle(render_fd, imported));
	TEST_SUCC(close(dmabuf));
	TEST_SUCC(destroy_dumb(dumb_handle));
}
END_TEST()

/*
 * Mesa's PowerVR winsys sizes an import with `lseek(fd, 0, SEEK_END)` before
 * `PRIME_FD_TO_HANDLE` (`pvr_drm_winsys_buffer_create_from_fd`). Linux
 * `dma_buf_llseek` returns the buffer size for SEEK_END with offset zero.
 * Verify that imported descriptors support this size query independently
 * of the GEM handle-table tests.
 */
FN_TEST(a_dmabuf_reports_its_size_through_lseek)
{
	uint32_t dumb_handle = 0, pitch = 0;
	uint64_t size = 0;
	int dmabuf = -1;

	SKIP_TEST_IF(!both_present);

	TEST_SUCC(create_dumb(&dumb_handle, &pitch, &size));
	TEST_SUCC(export_fd(card_fd, dumb_handle, &dmabuf));
	TEST_RES(dmabuf, _ret >= 0);

	TEST_RES(lseek(dmabuf, 0, SEEK_END), _ret == (off_t)size);
	TEST_ERRNO(lseek(dmabuf, 1, SEEK_END), EINVAL);
	TEST_RES(lseek(dmabuf, 0, SEEK_SET), _ret == 0);
	TEST_ERRNO(lseek(dmabuf, 1, SEEK_SET), EINVAL);
	TEST_ERRNO(lseek(dmabuf, 0, SEEK_CUR), EINVAL);

	TEST_SUCC(close(dmabuf));
	TEST_SUCC(destroy_dumb(dumb_handle));
}
END_TEST()

/*
 * The GPU half of the hand-off: memory the GPU allocated, presented to the DPU
 * as a framebuffer. This is the direction Mesa's VK_KHR_display WSI takes, and
 * the one where a wrong answer is invisible rather than loud, since the DPU
 * reads a physical address and reports nothing about what it found there.
 *
 * The buffer is written from the CPU first. That is not part of the hand-off,
 * but it proves the pages are real and writable before they are handed to a
 * device that will never say otherwise.
 */
FN_TEST(a_gpu_buffer_becomes_a_framebuffer_on_the_display_device)
{
	uint32_t bo_handle = 0, imported = 0;
	int dmabuf = -1;
	struct drm_pvr_ioctl_get_bo_mmap_offset_args offset = { 0 };
	struct drm_mode_fb_cmd2 fb = { 0 };
	unsigned int fb_id;
	void *mapping;

	SKIP_TEST_IF(!both_present);

	TEST_SUCC(create_bo(FB_SIZE, DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
			    &bo_handle));

	offset.handle = bo_handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_GET_BO_MMAP_OFFSET, &offset));
	mapping = TEST_RES(mmap(NULL, FB_SIZE, PROT_READ | PROT_WRITE,
				MAP_SHARED, render_fd, (off_t)offset.offset),
			   _ret != MAP_FAILED);
	if (mapping != MAP_FAILED) {
		memset(mapping, 0x5a, FB_SIZE);
		TEST_SUCC(munmap(mapping, FB_SIZE));
	}

	TEST_SUCC(export_fd(render_fd, bo_handle, &dmabuf));
	TEST_SUCC(import_handle(card_fd, dmabuf, &imported));
	TEST_RES(imported, _ret != 0);

	fb.width = FB_WIDTH;
	fb.height = FB_HEIGHT;
	fb.pixel_format = DRM_FORMAT_XRGB8888;
	fb.handles[0] = imported;
	fb.pitches[0] = FB_PITCH;
	/*
	 * EACCES here means something else holds DRM master on card0. Run this probe with the graphical session stopped; the compositor
	 * normally owns DRM master.
	 */
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_ADDFB2, &fb));
	TEST_RES(fb.fb_id, _ret != 0);

	/*
	 * Not scanned out. Whether the panel can actually display this is
	 * decided in atomic_check, which resolves the physical address, and
	 * committing it would replace the console with a grey rectangle that
	 * this test has no way to put back. actual scanout and rendering need a separate board test.
	 */
	fb_id = fb.fb_id;
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_RMFB, &fb_id));

	TEST_SUCC(close_handle(card_fd, imported));
	TEST_SUCC(close(dmabuf));
	TEST_SUCC(close_handle(render_fd, bo_handle));
}
END_TEST()

/* The mirror of the earlier one, on the side that has dumb buffers. */
FN_TEST(an_imported_buffer_has_no_cpu_mapping_on_the_display_side)
{
	uint32_t bo_handle = 0, imported = 0;
	int dmabuf = -1;
	struct drm_mode_map_dumb map = { 0 };

	SKIP_TEST_IF(!both_present);

	TEST_SUCC(create_bo(FB_SIZE, DRM_PVR_BO_ALLOW_CPU_USERSPACE_ACCESS,
			    &bo_handle));
	TEST_SUCC(export_fd(render_fd, bo_handle, &dmabuf));
	TEST_SUCC(import_handle(card_fd, dmabuf, &imported));

	map.handle = imported;
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_MAP_DUMB, &map), EINVAL);

	TEST_SUCC(close_handle(card_fd, imported));
	TEST_SUCC(close(dmabuf));
	TEST_SUCC(close_handle(render_fd, bo_handle));
}
END_TEST()

/*
 * The importer's reference has to outlive everything the exporter holds.
 *
 * A compositor does exactly this: it allocates, hands the descriptor over, and
 * drops its own handle. If the pages went back to the allocator at that point
 * the GPU would be writing into memory someone else owns — the worst shape of
 * this bug, because it corrupts a stranger rather than failing.
 */
FN_TEST(a_gpu_mapping_outlives_every_reference_the_exporter_held)
{
	uint32_t dumb_handle = 0, pitch = 0, imported = 0, vm_context = 0;
	uint64_t size = 0;
	int dmabuf = -1;
	struct drm_pvr_ioctl_create_vm_context_args create_vm = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy_vm = { 0 };
	struct drm_pvr_ioctl_vm_map_args map = { 0 };
	struct drm_pvr_ioctl_vm_unmap_args unmap = { 0 };

	SKIP_TEST_IF(!both_present);

	TEST_SUCC(create_dumb(&dumb_handle, &pitch, &size));
	TEST_SUCC(export_fd(card_fd, dumb_handle, &dmabuf));
	TEST_SUCC(import_handle(render_fd, dmabuf, &imported));

	TEST_SUCC(
		ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &create_vm));
	vm_context = create_vm.handle;

	map.vm_context_handle = vm_context;
	map.device_addr = ROGUE_GENERAL_HEAP_BASE;
	map.handle = imported;
	map.size = size;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_MAP, &map));

	/* Everything the allocating side had, released while the mapping stands. */
	TEST_SUCC(close(dmabuf));
	TEST_SUCC(destroy_dumb(dumb_handle));

	unmap.vm_context_handle = vm_context;
	unmap.device_addr = ROGUE_GENERAL_HEAP_BASE;
	unmap.size = size;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_VM_UNMAP, &unmap));

	destroy_vm.handle = vm_context;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT,
			&destroy_vm));
	TEST_SUCC(close_handle(render_fd, imported));
}
END_TEST()
