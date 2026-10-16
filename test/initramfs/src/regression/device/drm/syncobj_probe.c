// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: reproduce Mesa fence capability checks through raw ioctls.

/*
 * Mesa's `VkFence` capability probe, replicated without Mesa.
 *
 * Mesa does not ask a driver whether it can support fences; it finds out by
 * trying. `vk_drm_syncobj_get_type_from_provider()` creates a sync object with
 * DRM_SYNCOBJ_CREATE_SIGNALED, waits on it with WAIT_ALL and an absolute
 * deadline of zero, and grants VK_SYNC_FEATURE_CPU_WAIT only if that wait
 * returns success. `vk_fence.c`'s `get_fence_sync_type()` then *requires* that
 * feature, so a driver that answers anything else reports
 * VK_ERROR_INVALID_EXTERNAL_HANDLE (-1000072003) from vkCreateFence — an error
 * that names external handles for a fence created without any.
 *
 * Mesa silently stops probing if sync-object creation fails, so its public
 * error does not distinguish create failure from wait rejection. Exercise
 * the two ioctls directly to identify which operation failed, without
 * depending on libvulkan or libdrm.
 *
 * Structures and command numbers are transcribed from include/uapi/drm/drm.h
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

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L878>. */
struct drm_syncobj_create {
	uint32_t handle;
	uint32_t flags;
};

struct drm_syncobj_destroy {
	uint32_t handle;
	uint32_t pad;
};

struct drm_syncobj_wait {
	uint64_t handles;
	/* Absolute, not relative: a CLOCK_MONOTONIC deadline in nanoseconds. */
	int64_t timeout_nsec;
	uint32_t count_handles;
	uint32_t flags;
	uint32_t first_signaled;
	uint32_t pad;
	/* Appended in Linux 6.8; read only with WAIT_FLAGS_WAIT_DEADLINE. */
	uint64_t deadline_nsec;
};

/*
 * The ioctl number encodes the argument size. The dispatcher negotiates
 * compatible older/newer sizes by command number; these assertions still
 * check the caller layout independently of the port.
 *
 * Take expected sizes from the Linux UAPI header, independently of the port.
 * A kernel test comparing the port's structures with themselves cannot detect
 * a size mismatch with libdrm.
 *
 * Reference: include/uapi/drm/drm.h as of Linux 6.8, which is also what Mesa
 * vendors in include/drm-uapi/drm.h.
 */
_Static_assert(sizeof(struct drm_syncobj_create) == 8,
	       "drm_syncobj_create must be 8 bytes");
_Static_assert(sizeof(struct drm_syncobj_destroy) == 8,
	       "drm_syncobj_destroy must be 8 bytes");
_Static_assert(sizeof(struct drm_syncobj_wait) == 40,
	       "drm_syncobj_wait must be 40 bytes since Linux 6.8");

#define DRM_IOCTL_BASE 'd'

#define DRM_IOCTL_VERSION _IOWR(DRM_IOCTL_BASE, 0x00, struct drm_version)
#define DRM_IOCTL_SYNCOBJ_CREATE \
	_IOWR(DRM_IOCTL_BASE, 0xBF, struct drm_syncobj_create)
#define DRM_IOCTL_SYNCOBJ_DESTROY \
	_IOWR(DRM_IOCTL_BASE, 0xC0, struct drm_syncobj_destroy)
#define DRM_IOCTL_SYNCOBJ_WAIT \
	_IOWR(DRM_IOCTL_BASE, 0xC3, struct drm_syncobj_wait)

#define DRM_SYNCOBJ_CREATE_SIGNALED (1 << 0)
#define DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL (1 << 0)
#define DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT (1 << 1)

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
			"%s is a `%s` device, not `%s`; sync-object tests will be skipped\n",
			RENDER_DEVICE, name, PVR_DRIVER_NAME);
}
END_SETUP()

/*
 * SYNCOBJ_CREATE has to copy the allocated handle back out.
 *
 * Checked separately from the probe because the two failures are reported
 * identically by Mesa and are not distinguishable from inside the kernel
 * either: `syncobj_create` fills in the handle and returns success, and the
 * copy-back to user space happens *after* it returns, in `DriHandle::call`. A
 * handle that is still zero here means the ioctl succeeded and told nobody,
 * and Mesa's probe would abandon the whole fence type without ever waiting.
 */
FN_TEST(syncobj_create_reports_the_handle_it_allocated)
{
	struct drm_syncobj_create create = { 0 };
	struct drm_syncobj_destroy destroy = { 0 };

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_CREATE, &create));
	TEST_RES(create.handle, _ret != 0);

	destroy.handle = create.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_DESTROY, &destroy));
}
END_TEST()

/*
 * The probe itself, in the order and with the arguments Mesa uses.
 *
 * A failure of the wait here is a failure of vkCreateFence on the board, and
 * the errno says which half is wrong: ENOENT means the handle the create
 * reported is not the handle the kernel kept, EINVAL means the arguments were
 * rejected before the fences were consulted, and ETIME means an object created
 * signalled did not read as signalled.
 */
FN_TEST(mesas_fence_capability_probe_succeeds)
{
	struct drm_syncobj_create create = {
		.flags = DRM_SYNCOBJ_CREATE_SIGNALED
	};
	struct drm_syncobj_destroy destroy = { 0 };
	struct drm_syncobj_wait wait;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_CREATE, &create));
	TEST_RES(create.handle, _ret != 0);

	memset(&wait, 0, sizeof(wait));
	wait.handles = (uint64_t)(uintptr_t)&create.handle;
	wait.count_handles = 1;
	wait.timeout_nsec = 0;
	wait.flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL;

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_WAIT, &wait));

	destroy.handle = create.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_DESTROY, &destroy));
}
END_TEST()

/*
 * The other branch of the same wait, which decides `vkWaitForFences`.
 *
 * A pending object with a deadline in the past is ETIME (62), not ETIMEDOUT
 * (110): Vulkan turns exactly ETIME into VK_TIMEOUT and anything else into an
 * error, so a client asking "is it done yet?" would see a failure instead of
 * "not yet".
 *
 * WAIT_FOR_SUBMIT is required to get that far, and its absence is not a
 * timeout. An object created without SIGNALED has no fence at all, and waiting
 * on one without asking to wait for a fence to appear is EINVAL — upstream's
 * `drm_syncobj_find_fence` reports it and this port matches.
 */
FN_TEST(a_wait_that_reaches_its_deadline_expires)
{
	struct drm_syncobj_create create = { 0 };
	struct drm_syncobj_destroy destroy = { 0 };
	struct drm_syncobj_wait wait;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_CREATE, &create));

	memset(&wait, 0, sizeof(wait));
	wait.handles = (uint64_t)(uintptr_t)&create.handle;
	wait.count_handles = 1;
	wait.timeout_nsec = 0;
	wait.flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL |
		     DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT;

	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_WAIT, &wait), ETIME);

	destroy.handle = create.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_DESTROY, &destroy));
}
END_TEST()

/*
 * The same object, waited on *without* WAIT_FOR_SUBMIT: EINVAL, because it has
 * no fence to wait on. Pinned separately so that the distinction above is a
 * tested claim rather than a comment.
 */
FN_TEST(a_wait_on_an_object_with_no_fence_is_refused)
{
	struct drm_syncobj_create create = { 0 };
	struct drm_syncobj_destroy destroy = { 0 };
	struct drm_syncobj_wait wait;

	SKIP_TEST_IF(!is_powervr);

	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_CREATE, &create));

	memset(&wait, 0, sizeof(wait));
	wait.handles = (uint64_t)(uintptr_t)&create.handle;
	wait.count_handles = 1;
	wait.timeout_nsec = 0;
	wait.flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL;

	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_WAIT, &wait), EINVAL);

	destroy.handle = create.handle;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_DESTROY, &destroy));
}
END_TEST()

/*
 * A handle that was never allocated is ENOENT, matching
 * `drm_syncobj_array_find`. Included because it is the failure that a broken
 * copy-back would produce next: Mesa would wait on the zero it was left with.
 */
FN_TEST(a_wait_naming_an_unknown_handle_is_refused)
{
	uint32_t handle = 0xdeadbeef;
	struct drm_syncobj_wait wait;

	SKIP_TEST_IF(!is_powervr);

	memset(&wait, 0, sizeof(wait));
	wait.handles = (uint64_t)(uintptr_t)&handle;
	wait.count_handles = 1;
	wait.timeout_nsec = 0;
	wait.flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL;

	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_WAIT, &wait), ENOENT);
}
END_TEST()
