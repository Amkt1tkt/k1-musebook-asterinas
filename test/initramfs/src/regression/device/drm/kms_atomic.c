// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: empty atomic requests must not alter the display state.

#include <poll.h>

#include "../../common/test.h"
#include "common.h"

static int card_fd = -1;
static struct drm_mode_crtc initial_crtc;
static uint32_t encoder_id;

static int empty_atomic(int fd, uint32_t flags)
{
	struct drm_mode_atomic request = { .flags = flags };

	return ioctl(fd, DRM_IOCTL_MODE_ATOMIC, &request);
}

static int enable_atomic(int fd)
{
	struct drm_set_client_cap cap = {
		.capability = DRM_CLIENT_CAP_ATOMIC,
		.value = 1,
	};

	return ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &cap);
}

static int crtc_is_unchanged(void)
{
	struct drm_mode_crtc current = { .crtc_id = initial_crtc.crtc_id };

	if (ioctl(card_fd, DRM_IOCTL_MODE_GETCRTC, &current) < 0)
		return -1;
	return memcmp(&current, &initial_crtc, sizeof(current)) == 0;
}

FN_SETUP(open_atomic_device)
{
	/* Run without another display server holding the primary node. */
	card_fd = open_drm_node_or_skip(DRM_CARD_DEVICE);
	CHECK(ioctl(card_fd, DRM_IOCTL_SET_MASTER));
	CHECK(fcntl(card_fd, F_SETFL, O_NONBLOCK));

	uint32_t crtc_id = 0;
	struct drm_mode_card_res resources = {
		.crtc_id_ptr = (uintptr_t)&crtc_id,
		.count_crtcs = 1,
		.encoder_id_ptr = (uintptr_t)&encoder_id,
		.count_encoders = 1,
	};
	CHECK(ioctl(card_fd, DRM_IOCTL_MODE_GETRESOURCES, &resources));
	CHECK_WITH(resources.count_crtcs, _ret > 0);
	CHECK_WITH(resources.count_encoders, _ret > 0);
	initial_crtc.crtc_id = crtc_id;
	CHECK(ioctl(card_fd, DRM_IOCTL_MODE_GETCRTC, &initial_crtc));
}
END_SETUP()

FN_TEST(atomic_capability_is_required)
{
	TEST(empty_atomic(card_fd, DRM_MODE_ATOMIC_ALLOW_MODESET), EINVAL,
	     _ret == -1);
}
END_TEST()

FN_SETUP(enable_atomic_capability)
{
	/* Atomic support is required: a missing capability must not hide a regression. */
	CHECK(enable_atomic(card_fd));
}
END_SETUP()

FN_TEST(empty_atomic_requests_preserve_state)
{
	const uint32_t flags[] = {
		0,
		DRM_MODE_ATOMIC_ALLOW_MODESET,
		DRM_MODE_ATOMIC_TEST_ONLY,
		DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_ALLOW_MODESET,
		DRM_MODE_ATOMIC_NONBLOCK,
		DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_ATOMIC_ALLOW_MODESET,
		DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_ATOMIC_NONBLOCK |
			DRM_MODE_ATOMIC_ALLOW_MODESET,
	};

	for (size_t i = 0; i < sizeof(flags) / sizeof(flags[0]); i++) {
		fprintf(stderr, "empty atomic flags=%#x\n", flags[i]);
		TEST_RES(empty_atomic(card_fd, flags[i]), _ret == 0);
		TEST_RES(crtc_is_unchanged(), _ret == 1);
	}

	/* Zero counts do not authorize dereferencing any array pointer. */
	struct drm_mode_atomic request = {
		.flags = DRM_MODE_ATOMIC_ALLOW_MODESET,
		.objs_ptr = UINT64_MAX,
		.count_props_ptr = UINT64_MAX,
		.props_ptr = UINT64_MAX,
		.prop_values_ptr = UINT64_MAX,
	};
	TEST_RES(ioctl(card_fd, DRM_IOCTL_MODE_ATOMIC, &request), _ret == 0);
	TEST_RES(crtc_is_unchanged(), _ret == 1);
}
END_TEST()

FN_TEST(zero_properties_still_validate_the_object)
{
	uint32_t object = initial_crtc.crtc_id;
	uint32_t property_count = 0;
	struct drm_mode_atomic request = {
		.flags = DRM_MODE_ATOMIC_ALLOW_MODESET,
		.count_objs = 1,
		.objs_ptr = (uintptr_t)&object,
		.count_props_ptr = (uintptr_t)&property_count,
		.props_ptr = UINT64_MAX,
		.prop_values_ptr = UINT64_MAX,
	};
	TEST_RES(ioctl(card_fd, DRM_IOCTL_MODE_ATOMIC, &request), _ret == 0);
	TEST_RES(crtc_is_unchanged(), _ret == 1);

	object = UINT32_MAX;
	TEST(ioctl(card_fd, DRM_IOCTL_MODE_ATOMIC, &request), ENOENT,
	     _ret == -1);

	/* Encoders exist, but do not expose atomic properties. */
	object = encoder_id;
	TEST(ioctl(card_fd, DRM_IOCTL_MODE_ATOMIC, &request), ENOENT,
	     _ret == -1);
	TEST_RES(crtc_is_unchanged(), _ret == 1);
}
END_TEST()

FN_TEST(empty_atomic_cannot_request_a_completion_event)
{
	/* Linux prepare_signaling requires a CRTC for PAGE_FLIP_EVENT. */
	const uint32_t flags[] = {
		DRM_MODE_PAGE_FLIP_EVENT,
		DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_ALLOW_MODESET,
		DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK,
		DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_NONBLOCK |
			DRM_MODE_ATOMIC_ALLOW_MODESET,
		DRM_MODE_PAGE_FLIP_EVENT | DRM_MODE_ATOMIC_TEST_ONLY,
	};

	for (size_t i = 0; i < sizeof(flags) / sizeof(flags[0]); i++) {
		fprintf(stderr, "empty atomic event flags=%#x\n", flags[i]);
		TEST(empty_atomic(card_fd, flags[i]), EINVAL, _ret == -1);
	}

	struct pollfd event = { .fd = card_fd, .events = POLLIN };
	TEST_RES(poll(&event, 1, 20), _ret == 0 && event.revents == 0);
}
END_TEST()

FN_TEST(empty_atomic_keeps_argument_validation)
{
	TEST(empty_atomic(card_fd, UINT32_C(1) << 31), EINVAL, _ret == -1);
	TEST(empty_atomic(card_fd, DRM_MODE_PAGE_FLIP_ASYNC), EINVAL,
	     _ret == -1);

	struct drm_mode_atomic request = { .reserved = 1 };
	TEST(ioctl(card_fd, DRM_IOCTL_MODE_ATOMIC, &request), EINVAL,
	     _ret == -1);

	request = (struct drm_mode_atomic){ .count_objs = 1 };
	TEST(ioctl(card_fd, DRM_IOCTL_MODE_ATOMIC, &request), EFAULT,
	     _ret == -1);
	TEST_RES(crtc_is_unchanged(), _ret == 1);
}
END_TEST()

FN_TEST(empty_atomic_still_requires_master)
{
	int client_fd = CHECK(open(DRM_CARD_DEVICE, O_RDWR | O_NONBLOCK));
	CHECK(enable_atomic(client_fd));
	TEST(empty_atomic(client_fd, DRM_MODE_ATOMIC_ALLOW_MODESET), EACCES,
	     _ret == -1);
	TEST(empty_atomic(client_fd, DRM_MODE_ATOMIC_TEST_ONLY), EACCES,
	     _ret == -1);
	TEST_SUCC(close(client_fd));
	TEST_RES(crtc_is_unchanged(), _ret == 1);
}
END_TEST()

FN_SETUP(close_atomic_device)
{
	CHECK(ioctl(card_fd, DRM_IOCTL_DROP_MASTER));
	CHECK(close(card_fd));
}
END_SETUP()
