// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: exercise runtime power before rejecting an invalid job stream.

/*
 * Reach the firmware boot path through SUBMIT_JOBS.
 *
 * Creates a transfer context and submits a deliberately invalid four-byte
 * stream. The current nonempty submission path takes PowerRef before copying
 * and validating the stream, so the expected EINVAL checks that ordering.
 * This does not submit valid GPU work or prove a cold boot happened: firmware
 * may already be running, and EINVAL alone cannot identify every internal
 * branch. Kernel boot failures and the expected client error remain distinct.
 *
 * Structures and command numbers are transcribed from include/uapi/drm/pvr_drm.h
 * for the same reasons pvr_bo.c gives.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "../../common/test.h"

#define RENDER_DEVICE "/dev/dri/renderD128"
#define PVR_DRIVER_NAME "powervr"

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

/* Reference: <https://github.com/torvalds/linux/blob/fc02acf6ac0ccde0c805c2daa9148683cdd01ba8/include/uapi/drm/pvr_drm.h>. */
struct drm_pvr_obj_array {
	uint32_t stride;
	uint32_t count;
	uint64_t array;
};

struct drm_pvr_ioctl_create_vm_context_args {
	uint32_t handle;
	uint32_t _padding_4;
};

struct drm_pvr_ioctl_destroy_vm_context_args {
	uint32_t handle;
	uint32_t _padding_4;
};

struct drm_pvr_ioctl_create_context_args {
	uint32_t type;
	uint32_t flags;
	int32_t priority;
	uint32_t handle;
	uint64_t static_context_state;
	uint32_t static_context_state_len;
	uint32_t vm_context_handle;
	uint64_t callstack_addr;
};

struct drm_pvr_ioctl_destroy_context_args {
	uint32_t handle;
	uint32_t _padding_4;
};

struct drm_pvr_job {
	uint32_t type;
	uint32_t context_handle;
	uint32_t flags;
	uint32_t cmd_stream_len;
	uint64_t cmd_stream;
	struct drm_pvr_obj_array sync_ops;
	/* Flattened drm_pvr_hwrt_data_ref; preserves its two-u32 ABI. */
	uint32_t hwrt_set_handle;
	uint32_t hwrt_data_index;
};

struct drm_pvr_ioctl_submit_jobs_args {
	struct drm_pvr_obj_array jobs;
};

/*
 * The ioctl encoding includes the argument size; the dispatcher negotiates
 * compatible short/long structures by command number. These sizes come from
 * Mesa 26.1.6's include/drm-uapi/pvr_drm.h, not from the kernel port.
 */
_Static_assert(sizeof(struct drm_pvr_obj_array) == 16, "drm_pvr_obj_array");
_Static_assert(sizeof(struct drm_pvr_ioctl_create_vm_context_args) == 8,
	       "drm_pvr_ioctl_create_vm_context_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_destroy_vm_context_args) == 8,
	       "drm_pvr_ioctl_destroy_vm_context_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_create_context_args) == 40,
	       "drm_pvr_ioctl_create_context_args");
_Static_assert(sizeof(struct drm_pvr_ioctl_destroy_context_args) == 8,
	       "drm_pvr_ioctl_destroy_context_args");
_Static_assert(sizeof(struct drm_pvr_job) == 48, "drm_pvr_job");
_Static_assert(sizeof(struct drm_pvr_ioctl_submit_jobs_args) == 16,
	       "drm_pvr_ioctl_submit_jobs_args");

#define DRM_IOCTL_BASE 'd'
#define DRM_COMMAND_BASE 0x40

#define DRM_IOCTL_VERSION _IOWR(DRM_IOCTL_BASE, 0x00, struct drm_version)

#define DRM_IOCTL_PVR_CREATE_VM_CONTEXT                \
	_IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x03, \
	      struct drm_pvr_ioctl_create_vm_context_args)
#define DRM_IOCTL_PVR_DESTROY_VM_CONTEXT              \
	_IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x04, \
	     struct drm_pvr_ioctl_destroy_vm_context_args)
#define DRM_IOCTL_PVR_CREATE_CONTEXT                   \
	_IOWR(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x07, \
	      struct drm_pvr_ioctl_create_context_args)
#define DRM_IOCTL_PVR_DESTROY_CONTEXT                 \
	_IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x08, \
	     struct drm_pvr_ioctl_destroy_context_args)
#define DRM_IOCTL_PVR_SUBMIT_JOBS                     \
	_IOW(DRM_IOCTL_BASE, DRM_COMMAND_BASE + 0x0d, \
	     struct drm_pvr_ioctl_submit_jobs_args)

#define DRM_PVR_CTX_TYPE_TRANSFER_FRAG 2
#define DRM_PVR_CTX_PRIORITY_NORMAL 0
#define DRM_PVR_JOB_TYPE_TRANSFER_FRAG 3

static int render_fd = -1;
static int is_powervr;

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
 * Create a transfer context and call SUBMIT_JOBS with a stream that cannot
 * validate. EINVAL is the success of *this* test: the power reference — and
 * therefore the firmware boot — ran before the stream was refused.
 *
 * Inspect the bounded firmware boot diagnosis on failure. Routine successful
 * boot detail is not guaranteed on the serial console at the default log level.
 */
FN_TEST(submit_jobs_reaches_firmware_boot)
{
	struct drm_pvr_ioctl_create_vm_context_args vm = { 0 };
	struct drm_pvr_ioctl_create_context_args ctx = { 0 };
	struct drm_pvr_ioctl_destroy_context_args destroy_ctx = { 0 };
	struct drm_pvr_ioctl_destroy_vm_context_args destroy_vm = { 0 };
	struct drm_pvr_job job;
	struct drm_pvr_ioctl_submit_jobs_args submit;
	/* Too short for any job stream header; stream validation returns EINVAL. */
	uint32_t bad_stream = 0;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_VM_CONTEXT, &vm));
	TEST_RES(vm.handle, _ret != 0);

	ctx.type = DRM_PVR_CTX_TYPE_TRANSFER_FRAG;
	ctx.flags = 0;
	ctx.priority = DRM_PVR_CTX_PRIORITY_NORMAL;
	ctx.static_context_state = 0;
	ctx.static_context_state_len = 0;
	ctx.vm_context_handle = vm.handle;
	ctx.callstack_addr = 0;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_CREATE_CONTEXT, &ctx));
	TEST_RES(ctx.handle, _ret != 0);

	memset(&job, 0, sizeof(job));
	job.type = DRM_PVR_JOB_TYPE_TRANSFER_FRAG;
	job.context_handle = ctx.handle;
	job.flags = 0;
	job.cmd_stream_len = sizeof(bad_stream);
	job.cmd_stream = (uint64_t)(uintptr_t)&bad_stream;
	job.sync_ops.stride = 16;
	job.sync_ops.count = 0;
	job.sync_ops.array = 0;

	memset(&submit, 0, sizeof(submit));
	submit.jobs.stride = sizeof(job);
	submit.jobs.count = 1;
	submit.jobs.array = (uint64_t)(uintptr_t)&job;

	/*
	 * The power reference is taken before stream validation. EINVAL means
	 * boot ran and the client error was reported; anything else (especially
	 * ETIMEDOUT from wait_for_boot) means the firmware path itself failed.
	 */
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PVR_SUBMIT_JOBS, &submit),
		   EINVAL);

	destroy_ctx.handle = ctx.handle;
	TEST_SUCC(
		ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_CONTEXT, &destroy_ctx));

	destroy_vm.handle = vm.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_PVR_DESTROY_VM_CONTEXT,
			&destroy_vm));
}
END_TEST()
