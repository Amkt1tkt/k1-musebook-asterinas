// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: core DRM, GEM, PRIME and sync-object ABI regression.

/*
 * Smoke test for the DRM core and its two device nodes.
 *
 * The structures and command numbers below are transcribed from
 * include/uapi/drm/drm.h rather than included from it, and the ioctls are issued
 * raw rather than through libdrm. This keeps the test independent of shared libraries and checks
 * the userspace boundary using separately transcribed layouts.
 *
 * What this covers is the part of the interface every graphics client touches
 * first: DRM_IOCTL_VERSION's two-stage length protocol, DRM_IOCTL_GET_UNIQUE,
 * DRM_IOCTL_GET_CAP, DRM_IOCTL_SET_CLIENT_CAP, mastership, the rule that an
 * ioctl without DRM_RENDER_ALLOW must be refused on a render node, and the
 * whole dumb buffer round trip: create, map, write, read back, destroy. The
 * round trip exercises the ioctl decoder, GEM handle table, fake mmap offset
 * space, and demand-paged device mapping together.
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
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <unistd.h>

#include "../../common/test.h"

#define CARD_DEVICE "/dev/dri/card0"
#define RENDER_DEVICE "/dev/dri/renderD128"

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/Documentation/admin-guide/devices.txt>. */
#define DRM_MAJOR 226
#define CARD_MINOR 0
#define RENDER_MINOR 128

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

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L156>. */
struct drm_unique {
	size_t unique_len;
	char *unique;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L200>. */
struct drm_set_version {
	int drm_di_major;
	int drm_di_minor;
	int drm_dd_major;
	int drm_dd_minor;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L778>. */
struct drm_get_cap {
	uint64_t capability;
	uint64_t value;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L846>. */
struct drm_set_client_cap {
	uint64_t capability;
	uint64_t value;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L780>. */
struct drm_gem_close {
	uint32_t handle;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L789>. */
struct drm_gem_flink {
	uint32_t handle;
	uint32_t name;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L801>. */
struct drm_gem_open {
	uint32_t name;
	uint32_t handle;
	uint64_t size;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L943>. */
struct drm_mode_create_dumb {
	uint32_t height;
	uint32_t width;
	uint32_t bpp;
	uint32_t flags;
	uint32_t handle;
	uint32_t pitch;
	uint64_t size;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L953>. */
struct drm_mode_map_dumb {
	uint32_t handle;
	uint32_t pad;
	uint64_t offset;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L967>. */
struct drm_mode_destroy_dumb {
	uint32_t handle;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L853>. */
struct drm_prime_handle {
	uint32_t handle;
	uint32_t flags;
	int32_t fd;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L863>. */
struct drm_syncobj_create {
	uint32_t handle;
	uint32_t flags;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L869>. */
struct drm_syncobj_destroy {
	uint32_t handle;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L876>. */
struct drm_syncobj_handle {
	uint32_t handle;
	uint32_t flags;
	int32_t fd;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L896>. */
/* Linux 6.6 prefix: syncobj_probe also checks the newer deadline layout. */
struct drm_syncobj_wait {
	uint64_t handles;
	int64_t timeout_nsec;
	uint32_t count_handles;
	uint32_t flags;
	uint32_t first_signaled;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L908>. */
/* Linux 6.6 prefix; these waits do not request the appended deadline field. */
struct drm_syncobj_timeline_wait {
	uint64_t handles;
	uint64_t points;
	int64_t timeout_nsec;
	uint32_t count_handles;
	uint32_t flags;
	uint32_t first_signaled;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L931>. */
struct drm_syncobj_transfer {
	uint32_t src_handle;
	uint32_t dst_handle;
	uint64_t src_point;
	uint64_t dst_point;
	uint32_t flags;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L940>. */
struct drm_syncobj_array {
	uint64_t handles;
	uint32_t count_handles;
	uint32_t pad;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L947>. */
struct drm_syncobj_timeline_array {
	uint64_t handles;
	uint64_t points;
	uint32_t count_handles;
	uint32_t flags;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L993>. */
#define DRM_IOCTL_BASE 'd'
#define DRM_IOCTL_VERSION _IOWR(DRM_IOCTL_BASE, 0x00, struct drm_version)
#define DRM_IOCTL_GET_UNIQUE _IOWR(DRM_IOCTL_BASE, 0x01, struct drm_unique)
#define DRM_IOCTL_SET_VERSION \
	_IOWR(DRM_IOCTL_BASE, 0x07, struct drm_set_version)
#define DRM_IOCTL_GEM_CLOSE _IOW(DRM_IOCTL_BASE, 0x09, struct drm_gem_close)
#define DRM_IOCTL_GEM_FLINK _IOWR(DRM_IOCTL_BASE, 0x0a, struct drm_gem_flink)
#define DRM_IOCTL_GEM_OPEN _IOWR(DRM_IOCTL_BASE, 0x0b, struct drm_gem_open)
#define DRM_IOCTL_GET_CAP _IOWR(DRM_IOCTL_BASE, 0x0c, struct drm_get_cap)
#define DRM_IOCTL_SET_CLIENT_CAP \
	_IOW(DRM_IOCTL_BASE, 0x0d, struct drm_set_client_cap)
#define DRM_IOCTL_SET_MASTER _IO(DRM_IOCTL_BASE, 0x1e)
#define DRM_IOCTL_DROP_MASTER _IO(DRM_IOCTL_BASE, 0x1f)
#define DRM_IOCTL_MODE_CREATE_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB2, struct drm_mode_create_dumb)
#define DRM_IOCTL_MODE_MAP_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB3, struct drm_mode_map_dumb)
#define DRM_IOCTL_MODE_DESTROY_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB4, struct drm_mode_destroy_dumb)
#define DRM_IOCTL_PRIME_HANDLE_TO_FD \
	_IOWR(DRM_IOCTL_BASE, 0x2d, struct drm_prime_handle)
#define DRM_IOCTL_PRIME_FD_TO_HANDLE \
	_IOWR(DRM_IOCTL_BASE, 0x2e, struct drm_prime_handle)
#define DRM_IOCTL_SYNCOBJ_CREATE \
	_IOWR(DRM_IOCTL_BASE, 0xBF, struct drm_syncobj_create)
#define DRM_IOCTL_SYNCOBJ_DESTROY \
	_IOWR(DRM_IOCTL_BASE, 0xC0, struct drm_syncobj_destroy)
#define DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD \
	_IOWR(DRM_IOCTL_BASE, 0xC1, struct drm_syncobj_handle)
#define DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE \
	_IOWR(DRM_IOCTL_BASE, 0xC2, struct drm_syncobj_handle)
#define DRM_IOCTL_SYNCOBJ_WAIT \
	_IOWR(DRM_IOCTL_BASE, 0xC3, struct drm_syncobj_wait)
#define DRM_IOCTL_SYNCOBJ_RESET \
	_IOWR(DRM_IOCTL_BASE, 0xC4, struct drm_syncobj_array)
#define DRM_IOCTL_SYNCOBJ_SIGNAL \
	_IOWR(DRM_IOCTL_BASE, 0xC5, struct drm_syncobj_array)
#define DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT \
	_IOWR(DRM_IOCTL_BASE, 0xCA, struct drm_syncobj_timeline_wait)
#define DRM_IOCTL_SYNCOBJ_QUERY \
	_IOWR(DRM_IOCTL_BASE, 0xCB, struct drm_syncobj_timeline_array)
#define DRM_IOCTL_SYNCOBJ_TRANSFER \
	_IOWR(DRM_IOCTL_BASE, 0xCC, struct drm_syncobj_transfer)
#define DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL \
	_IOWR(DRM_IOCTL_BASE, 0xCD, struct drm_syncobj_timeline_array)

/* DRM_IOCTL_IRQ_BUSID, one of the legacy interfaces we decline to implement. */
#define DRM_IOCTL_IRQ_BUSID _IOWR(DRM_IOCTL_BASE, 0x03, int[4])

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L634>. */
#define DRM_CAP_DUMB_BUFFER 0x1
#define DRM_CAP_DUMB_PREFERRED_DEPTH 0x3
#define DRM_CAP_DUMB_PREFER_SHADOW 0x4
#define DRM_CAP_PRIME 0x5
#define DRM_CAP_TIMESTAMP_MONOTONIC 0x6
#define DRM_CAP_SYNCOBJ 0x13
#define DRM_CAP_SYNCOBJ_TIMELINE 0x14

#define DRM_PRIME_CAP_IMPORT 0x1
#define DRM_PRIME_CAP_EXPORT 0x2

#define DRM_CLIENT_CAP_UNIVERSAL_PLANES 2
#define DRM_CLIENT_CAP_ATOMIC 3

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L851>. */
#define DRM_CLOEXEC O_CLOEXEC
#define DRM_RDWR O_RDWR

#define DRM_SYNCOBJ_CREATE_SIGNALED (1 << 0)
#define DRM_SYNCOBJ_WAIT_FLAGS_WAIT_ALL (1 << 0)
#define DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT (1 << 1)
#define DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED (1 << 0)

#define STRING_BUFFER_LEN 128

/* The name the driver behind the primary node reports, read at startup.
 *
 * Not a constant, because which driver owns `card0` depends on the machine:
 * under QEMU nothing else registers and the software driver takes it, reporting
 * "aster", while on the K1 the DPU driver in `aster-framebuffer` gets there
 * first and reports "spacemit". Both are correct, and a test that hardcodes
 * either one is testing the machine rather than the two-stage protocol it means
 * to check. See kernel/core/comps/k1-musebook/pvr/ABI.md. */
static char primary_driver[STRING_BUFFER_LEN];
static size_t primary_driver_len;

#define PAGE_SIZE 4096

/* Big enough to span several pages, small enough to be cheap. */
#define DUMB_WIDTH 64
#define DUMB_HEIGHT 64
#define DUMB_BPP 32

static int card_fd = -1;
static int render_fd = -1;

/* The byte a given page of a dumb buffer should hold once written. A function
 * of the page index, so that a mapping that resolved the same page twice, or
 * resolved the wrong one, reads back the wrong value instead of looking fine. */
static unsigned char pattern_of(size_t page)
{
	return (unsigned char)(0x40 + (page % 173));
}

static void fill_pattern(unsigned char *mem, size_t size)
{
	for (size_t page = 0; page * PAGE_SIZE < size; ++page) {
		volatile unsigned char *p = mem + page * PAGE_SIZE;

		p[0] = pattern_of(page);
		p[PAGE_SIZE / 2] = pattern_of(page);
		p[PAGE_SIZE - 1] = pattern_of(page);
	}
}

/* Returns the number of pages that do not read back what was written. */
static int check_pattern(const unsigned char *mem, size_t size)
{
	int mismatches = 0;

	for (size_t page = 0; page * PAGE_SIZE < size; ++page) {
		const volatile unsigned char *p = mem + page * PAGE_SIZE;
		unsigned char expected = pattern_of(page);

		if (p[0] == expected && p[PAGE_SIZE / 2] == expected &&
		    p[PAGE_SIZE - 1] == expected)
			continue;

		fprintf(stderr,
			"page %zu of the dumb buffer does not read back\n",
			page);
		mismatches++;
	}

	return mismatches;
}

static int map_dumb(int fd, uint32_t handle, uint64_t *offset)
{
	struct drm_mode_map_dumb arg = { .handle = handle };

	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &arg) < 0)
		return -1;

	*offset = arg.offset;
	return 0;
}

static int get_cap(int fd, uint64_t capability, uint64_t *value)
{
	struct drm_get_cap arg = { .capability = capability, .value = 0 };

	if (ioctl(fd, DRM_IOCTL_GET_CAP, &arg) < 0)
		return -1;

	*value = arg.value;
	return 0;
}

static int set_client_cap(int fd, uint64_t capability, uint64_t value)
{
	struct drm_set_client_cap arg = { .capability = capability,
					  .value = value };

	return ioctl(fd, DRM_IOCTL_SET_CLIENT_CAP, &arg);
}

/*
 * This generic core test permits a driver without atomic support.
 * Consequently, passing it does not prove atomic modesetting is supported.
 */
static int set_atomic_cap(int fd)
{
	int ret = set_client_cap(fd, DRM_CLIENT_CAP_ATOMIC, 1);

	if (ret < 0 && errno == EOPNOTSUPP) {
		errno = 0;
		return 0;
	}
	return ret;
}

static int check_device_number(int fd, unsigned int expected_minor)
{
	struct stat status;

	if (fstat(fd, &status) < 0)
		return -1;

	if (major(status.st_rdev) != DRM_MAJOR ||
	    minor(status.st_rdev) != expected_minor) {
		fprintf(stderr,
			"expected device %u:%u but the node reports %u:%u\n",
			DRM_MAJOR, expected_minor, major(status.st_rdev),
			minor(status.st_rdev));
		errno = 0;
		return -1;
	}

	return 0;
}

FN_SETUP(open_drm_nodes)
{
	card_fd = open(CARD_DEVICE, O_RDWR);
	if (card_fd < 0) {
		if (errno == ENOENT || errno == ENODEV || errno == ENXIO) {
			fprintf(stderr, "DRM tests skipped: %s (%s)\n",
				CARD_DEVICE, strerror(errno));
			exit(EXIT_SUCCESS);
		}
		fprintf(stderr, "fatal error: open('%s') failed: %s\n",
			CARD_DEVICE, strerror(errno));
		exit(EXIT_FAILURE);
	}

	render_fd = CHECK(open(RENDER_DEVICE, O_RDWR));

	/* Learn who owns the primary node before anything asserts about it. Uses
	 * the same two-stage protocol the tests below check, which is safe: a
	 * failure here is reported as a fatal setup error rather than as a
	 * confusing mismatch in every later test. */
	struct drm_version probe = { 0 };
	if (ioctl(card_fd, DRM_IOCTL_VERSION, &probe) < 0 ||
	    probe.name_len == 0 || probe.name_len >= sizeof(primary_driver)) {
		fprintf(stderr,
			"fatal error: could not learn the primary driver name\n");
		exit(EXIT_FAILURE);
	}

	struct drm_version named = { 0 };
	named.name_len = probe.name_len;
	named.name = primary_driver;
	if (ioctl(card_fd, DRM_IOCTL_VERSION, &named) < 0) {
		fprintf(stderr,
			"fatal error: could not read the driver name\n");
		exit(EXIT_FAILURE);
	}
	primary_driver[probe.name_len] = '\0';
	primary_driver_len = probe.name_len;
	fprintf(stderr, "primary node is driven by \"%s\"\n", primary_driver);
}
END_SETUP()

FN_TEST(device_numbers)
{
	TEST_RES(check_device_number(card_fd, CARD_MINOR), _ret == 0);
	TEST_RES(check_device_number(render_fd, RENDER_MINOR), _ret == 0);
}
END_TEST()

/*
 * DRM_IOCTL_VERSION is the two-stage protocol libdrm's drmGetVersion depends on:
 * the first call carries zero-length buffers purely to learn the lengths, and the
 * kernel must report them without writing anything.
 */
FN_TEST(version_two_stage_protocol)
{
	struct drm_version probe;
	char name[STRING_BUFFER_LEN];
	char date[STRING_BUFFER_LEN];
	char desc[STRING_BUFFER_LEN];

	memset(&probe, 0, sizeof(probe));
	TEST_RES(ioctl(card_fd, DRM_IOCTL_VERSION, &probe),
		 _ret == 0 && probe.name_len == primary_driver_len &&
			 probe.date_len > 0 && probe.desc_len > 0);
	TEST_RES(probe.version_major, probe.version_major >= 1);

	// The reported lengths must fit the buffers this test allocated, or the
	// second stage below would be checking the wrong thing.
	SKIP_TEST_IF(probe.name_len >= STRING_BUFFER_LEN ||
		     probe.date_len >= STRING_BUFFER_LEN ||
		     probe.desc_len >= STRING_BUFFER_LEN);

	// Second stage: buffers sized from the first answer. The kernel does not
	// NUL-terminate, so pre-fill and terminate at the reported length.
	memset(name, 0, sizeof(name));
	memset(date, 0, sizeof(date));
	memset(desc, 0, sizeof(desc));

	struct drm_version version = {
		.name_len = probe.name_len,
		.name = name,
		.date_len = probe.date_len,
		.date = date,
		.desc_len = probe.desc_len,
		.desc = desc,
	};

	TEST_RES(ioctl(card_fd, DRM_IOCTL_VERSION, &version),
		 _ret == 0 && version.name_len == probe.name_len);

	TEST_RES(version.name_len, _ret < sizeof(name));
	SKIP_TEST_IF(version.name_len >= sizeof(name));
	name[version.name_len] = '\0';
	TEST_RES(version.date_len, _ret < sizeof(date));
	TEST_RES(version.desc_len, _ret < sizeof(desc));
	SKIP_TEST_IF(version.date_len >= sizeof(date) ||
		     version.desc_len >= sizeof(desc));
	date[version.date_len] = '\0';
	desc[version.desc_len] = '\0';

	fprintf(stderr, "driver %s %d.%d.%d (%s): %s\n", name,
		version.version_major, version.version_minor,
		version.version_patchlevel, date, desc);

	TEST_RES(strcmp(name, primary_driver), _ret == 0);

	// A short buffer gets a truncated copy but still learns the full length,
	// which is what lets a caller retry instead of guessing.
	char truncated[STRING_BUFFER_LEN];

	memset(truncated, 'x', sizeof(truncated));
	struct drm_version short_buffer = {
		.name_len = 2,
		.name = truncated,
	};

	TEST_RES(ioctl(card_fd, DRM_IOCTL_VERSION, &short_buffer),
		 _ret == 0 && short_buffer.name_len == probe.name_len);
	TEST_RES(memcmp(truncated, primary_driver, 2),
		 _ret == 0 && truncated[2] == 'x');
}
END_TEST()

/* Both nodes must answer DRM_IOCTL_VERSION, which carries DRM_RENDER_ALLOW. */
/*
 * The render node answers VERSION too — but not necessarily with the same driver
 * name as the primary node.
 *
 * Query the render node independently: K1 uses separate drivers for card0
 * (display) and renderD128 (PowerVR), so their VERSION lengths may differ.
 */
FN_TEST(version_on_render_node)
{
	struct drm_version probe;
	char name[STRING_BUFFER_LEN];

	memset(&probe, 0, sizeof(probe));
	TEST_RES(ioctl(render_fd, DRM_IOCTL_VERSION, &probe),
		 _ret == 0 && probe.name_len > 0 &&
			 probe.name_len < STRING_BUFFER_LEN);

	SKIP_TEST_IF(probe.name_len == 0 ||
		     probe.name_len >= STRING_BUFFER_LEN);
	struct drm_version version = { .name_len = probe.name_len,
				       .name = name };

	TEST_RES(ioctl(render_fd, DRM_IOCTL_VERSION, &version),
		 _ret == 0 && version.name_len == probe.name_len);
	TEST_RES(version.name_len, _ret < sizeof(name));
	SKIP_TEST_IF(version.name_len >= sizeof(name));
	name[version.name_len] = '\0';
	fprintf(stderr, "render node is driven by \"%s\"\n", name);
	TEST_RES(strlen(name), _ret == probe.name_len);
}
END_TEST()

/*
 * DRM_IOCTL_GET_UNIQUE is all or nothing, unlike VERSION: the bus ID is written
 * only if the caller's buffer can hold all of it.
 *
 * And it says nothing at all until the client has asked for interface version 1.1
 * with SET_VERSION. That is upstream's behaviour, and libdrm depends on it:
 * `drmOpenByName` treats a non-empty bus ID on a fresh descriptor as "this is a
 * PCI device I should match by bus ID", finds no match, and fails with ENOENT —
 * which is exactly how `modetest -M spacemit` failed before the driver started
 * withholding it.
 */
FN_TEST(get_unique)
{
	struct drm_unique probe = { .unique_len = 0, .unique = NULL };
	char unique[STRING_BUFFER_LEN];

	// Before SET_VERSION: present, and empty.
	TEST_RES(ioctl(card_fd, DRM_IOCTL_GET_UNIQUE, &probe),
		 _ret == 0 && probe.unique_len == 0);

	struct drm_set_version sv = { .drm_di_major = 1,
				      .drm_di_minor = 1,
				      .drm_dd_major = -1,
				      .drm_dd_minor = -1 };
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_SET_VERSION, &sv));

	// After it: the bus ID appears.
	memset(&probe, 0, sizeof(probe));
	TEST_RES(ioctl(card_fd, DRM_IOCTL_GET_UNIQUE, &probe),
		 _ret == 0 && probe.unique_len > 0);
	SKIP_TEST_IF(probe.unique_len == 0 ||
		     probe.unique_len >= STRING_BUFFER_LEN);

	memset(unique, 'x', sizeof(unique));
	struct drm_unique short_buffer = { .unique_len = probe.unique_len - 1,
					   .unique = unique };

	TEST_RES(ioctl(card_fd, DRM_IOCTL_GET_UNIQUE, &short_buffer),
		 _ret == 0 && short_buffer.unique_len == probe.unique_len &&
			 unique[0] == 'x');

	struct drm_unique exact = { .unique_len = probe.unique_len,
				    .unique = unique };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_GET_UNIQUE, &exact));
	TEST_RES(exact.unique_len, _ret < sizeof(unique));
	SKIP_TEST_IF(exact.unique_len >= sizeof(unique));
	unique[exact.unique_len] = '\0';
	fprintf(stderr, "bus ID %s\n", unique);
}
END_TEST()

FN_TEST(capabilities)
{
	uint64_t value = 0;

	// A monotonic vblank timestamp is unconditional in Linux and here.
	TEST_RES(get_cap(card_fd, DRM_CAP_TIMESTAMP_MONOTONIC, &value),
		 _ret == 0 && value == 1);

	TEST_RES(get_cap(card_fd, DRM_CAP_DUMB_BUFFER, &value),
		 _ret == 0 && value == 1);
	TEST_RES(get_cap(card_fd, DRM_CAP_DUMB_PREFERRED_DEPTH, &value),
		 _ret == 0 && value == 24);
	// The scanout buffer is mapped directly rather than shadowed.
	TEST_RES(get_cap(card_fd, DRM_CAP_DUMB_PREFER_SHADOW, &value),
		 _ret == 0 && value == 0);

	// Both PRIME directions are implemented and must be advertised to clients.
	TEST_RES(get_cap(card_fd, DRM_CAP_PRIME, &value),
		 _ret == 0 && value == (DRM_PRIME_CAP_IMPORT |
					DRM_PRIME_CAP_EXPORT));

	// Both sync-object capabilities are claimed, and the eleven ioctls
	// behind them work; see the sync-object tests below. The capability is
	// the feature bit, so answering one here and refusing the ioctls there
	// would be the failure this pair guards against.
	// Asked of the *render* node, which is where the sync-object ioctls are
	// used and therefore the only node whose answer has to be one. The primary
	// node may belong to a display driver that claims neither -- the K1's DPU
	// does not -- and that is not this test's business.
	TEST_RES(get_cap(render_fd, DRM_CAP_SYNCOBJ, &value),
		 _ret == 0 && value == 1);
	TEST_RES(get_cap(render_fd, DRM_CAP_SYNCOBJ_TIMELINE, &value),
		 _ret == 0 && value == 1);

	// A capability nobody has heard of is EINVAL, not a silent zero.
	TEST_ERRNO(get_cap(card_fd, 0xdeadbeef, &value), EINVAL);

	// GET_CAP carries DRM_RENDER_ALLOW, so the render node answers too.
	TEST_RES(get_cap(render_fd, DRM_CAP_TIMESTAMP_MONOTONIC, &value),
		 _ret == 0 && value == 1);
}
END_TEST()

FN_TEST(client_capabilities)
{
	TEST_SUCC(set_client_cap(card_fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 1));
	TEST_SUCC(set_atomic_cap(card_fd));

	// Linux restricts this capability to zero or one.
	TEST_ERRNO(set_client_cap(card_fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES, 2),
		   EINVAL);
	TEST_ERRNO(set_client_cap(card_fd, 0xdeadbeef, 1), EINVAL);
}
END_TEST()

/*
 * The reason render nodes exist. DRM_IOCTL_GET_UNIQUE, DRM_IOCTL_SET_CLIENT_CAP
 * and the mastership ioctls carry no DRM_RENDER_ALLOW in Linux, so reaching them
 * through renderD128 must fail with EACCES no matter who asks.
 */
FN_TEST(render_node_refuses_primary_only_ioctls)
{
	struct drm_unique unique = { .unique_len = 0, .unique = NULL };

	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_GET_UNIQUE, &unique), EACCES);
	TEST_ERRNO(set_client_cap(render_fd, DRM_CLIENT_CAP_UNIVERSAL_PLANES,
				  1),
		   EACCES);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_SET_MASTER, 0), EACCES);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_DROP_MASTER, 0), EACCES);
}
END_TEST()

/*
 * The sole client on the primary node is master, so it can give mastership up and
 * take it back. That is the sequence a compositor performs around a VT switch.
 */
FN_TEST(mastership)
{
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_DROP_MASTER, 0));
	// Dropping twice is EINVAL: there is nothing left to drop.
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_DROP_MASTER, 0), EINVAL);
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_SET_MASTER, 0));
	// Claiming it while already holding it is a no-op, not an error.
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_SET_MASTER, 0));
}
END_TEST()

/*
 * Create a dumb buffer, obtain its fake mmap offset, map it, write and read
 * back pixels, then destroy it.
 *
 * Each step exercises a different layer, so a failure localizes itself:
 * MODE_CREATE_DUMB is the GEM handle table, MODE_MAP_DUMB is the fake offset
 * allocator, mmap is the demand-paged device mapping, and the
 * read-back is whether the pages the fault handler produced are the object's
 * own.
 */
FN_TEST(dumb_buffer_round_trip)
{
	struct drm_mode_create_dumb create = {
		.width = DUMB_WIDTH,
		.height = DUMB_HEIGHT,
		.bpp = DUMB_BPP,
	};

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &create));

	// The pitch may be rounded up for the hardware, so a client has to use
	// what it is given rather than width * bpp / 8. The size is at least
	// that pitch times the height, and page aligned, because it is about to
	// be an mmap length.
	TEST_RES(create.handle, _ret != 0);
	TEST_RES(create.pitch, _ret >= DUMB_WIDTH * DUMB_BPP / 8);
	TEST_RES(create.size, _ret >= (uint64_t)create.pitch * DUMB_HEIGHT &&
				      _ret % PAGE_SIZE == 0);

	uint64_t offset = 0;

	TEST_SUCC(map_dumb(card_fd, create.handle, &offset));
	TEST_RES(offset, _ret % PAGE_SIZE == 0);

	// Asking twice must give the same answer: user space caches the offset
	// and would otherwise map a different buffer the second time.
	uint64_t offset_again = 0;

	TEST_SUCC(map_dumb(card_fd, create.handle, &offset_again));
	TEST_RES(offset_again, _ret == offset);

	size_t size = (size_t)create.size;
	unsigned char *mem = TEST_RES(mmap(NULL, size, PROT_READ | PROT_WRITE,
					   MAP_SHARED, card_fd, offset),
				      _ret != MAP_FAILED);
	SKIP_TEST_IF(mem == MAP_FAILED);

	// Writing every page is what makes the demand-paged mapping resolve
	// each of them; reading them back is what proves the fault handler
	// handed out distinct pages rather than the same one over and over.
	fill_pattern(mem, size);
	TEST_RES(check_pattern(mem, size), _ret == 0);

	// A second mapping of the same offset is the same memory, which is the
	// property every compositor depends on when it hands a buffer to
	// another process.
	unsigned char *shared =
		TEST_RES(mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
			      card_fd, offset),
			 _ret != MAP_FAILED);
	SKIP_TEST_IF(shared == MAP_FAILED);

	TEST_RES(check_pattern(shared, size), _ret == 0);
	shared[0] = 0xa5;
	TEST_RES(mem[0], _ret == 0xa5);
	TEST_SUCC(munmap(shared, size));

	// The pages belong to the buffer, not to the mapping. Unmapping and
	// mapping again must find them unchanged; if the fault handler had
	// handed out its only reference, this would read zeroes.
	mem[0] = pattern_of(0);
	TEST_SUCC(munmap(mem, size));

	mem = TEST_RES(mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
			    card_fd, offset),
		       _ret != MAP_FAILED);
	SKIP_TEST_IF(mem == MAP_FAILED);
	TEST_RES(check_pattern(mem, size), _ret == 0);
	TEST_SUCC(munmap(mem, size));

	struct drm_mode_destroy_dumb destroy = { .handle = create.handle };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy));

	// The handle is gone: mapping it again finds nothing, and destroying it
	// twice is EINVAL, which is the errno drm_gem_handle_delete reports.
	TEST_ERRNO(map_dumb(card_fd, create.handle, &offset), ENOENT);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy),
		   EINVAL);
}
END_TEST()

/* Private mappings must copy current pixels before becoming ordinary CPU pages. */
FN_TEST(private_dumb_mapping_copies_current_pixels_and_isolates_writes)
{
	struct drm_mode_create_dumb create = {
		.width = DUMB_WIDTH,
		.height = DUMB_HEIGHT,
		.bpp = DUMB_BPP,
	};
	uint64_t offset = 0;
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &create));
	TEST_SUCC(map_dumb(card_fd, create.handle, &offset));
	unsigned char *shared =
		CHECK_WITH(mmap(NULL, PAGE_SIZE, PROT_READ | PROT_WRITE,
				MAP_SHARED, card_fd, offset),
			   _ret != MAP_FAILED);
	unsigned char *private =
		CHECK_WITH(mmap(NULL, PAGE_SIZE, PROT_READ | PROT_WRITE,
				MAP_PRIVATE, card_fd, offset),
			   _ret != MAP_FAILED);

	memset(shared, 0x37, PAGE_SIZE);
	TEST_RES(private[0], _ret == 0x37);
	private[0] = 0xab;
	TEST_RES(private[0], _ret == 0xab);
	TEST_RES(shared[0], _ret == 0x37);
	int copied = 1;
	for (size_t i = 1; i < PAGE_SIZE; ++i)
		if (private[i] != 0x37)
			copied = 0;
	TEST_RES(copied, _ret == 1);

	shared[PAGE_SIZE / 2] = 0x61;
	TEST_RES(private[PAGE_SIZE / 2], _ret == 0x37);
	TEST_SUCC(mprotect(private, PAGE_SIZE, PROT_READ));
	TEST_SUCC(mprotect(private, PAGE_SIZE, PROT_READ | PROT_WRITE));
	private[1] = 0xc9;
	TEST_RES(private[0], _ret == 0xab);
	TEST_RES(private[1], _ret == 0xc9);
	TEST_RES(shared[1], _ret == 0x37);

	TEST_SUCC(munmap(private, PAGE_SIZE));
	TEST_SUCC(munmap(shared, PAGE_SIZE));
	struct drm_mode_destroy_dumb destroy = { .handle = create.handle };
	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy));
}
END_TEST()

/* A buffer whose geometry cannot be represented must be refused, not rounded. */
FN_TEST(dumb_buffer_rejects_bad_arguments)
{
	struct drm_mode_create_dumb zero_width = { .width = 0,
						   .height = 16,
						   .bpp = 32 };
	struct drm_mode_create_dumb zero_height = { .width = 16,
						    .height = 0,
						    .bpp = 32 };
	struct drm_mode_create_dumb zero_bpp = { .width = 16,
						 .height = 16,
						 .bpp = 0 };
	struct drm_mode_create_dumb overflowing = { .width = 0xffffffff,
						    .height = 0xffffffff,
						    .bpp = 32 };
	struct drm_mode_create_dumb unknown_flag = {
		.width = 16, .height = 16, .bpp = 32, .flags = 1
	};

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &zero_width),
		   EINVAL);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &zero_height),
		   EINVAL);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &zero_bpp),
		   EINVAL);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &overflowing),
		   EINVAL);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &unknown_flag),
		   EINVAL);

	// A handle nobody owns. GEM_CLOSE and MODE_DESTROY_DUMB report EINVAL
	// while everything that merely looks a handle up reports ENOENT; both
	// spellings are Linux's and user space branches on them.
	uint64_t offset = 0;
	struct drm_gem_close close_bad = { .handle = 0xdead };
	struct drm_mode_destroy_dumb destroy_bad = { .handle = 0xdead };

	TEST_ERRNO(map_dumb(card_fd, 0xdead, &offset), ENOENT);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_GEM_CLOSE, &close_bad), EINVAL);
	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy_bad),
		   EINVAL);
}
END_TEST()

/*
 * A flink name is a device-wide alias for a buffer. GEM_OPEN resolves it into a
 * handle of its own, so the two handles are the same memory and each holds the
 * buffer alive independently.
 *
 * Both ioctls carry DRM_AUTH, so this test issues them through the master fd.
 */
FN_TEST(gem_flink_and_open_name_a_buffer)
{
	struct drm_mode_create_dumb create = { .width = 32,
					       .height = 32,
					       .bpp = 32 };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &create));

	struct drm_gem_flink flink = { .handle = create.handle };

	TEST_RES(ioctl(card_fd, DRM_IOCTL_GEM_FLINK, &flink),
		 _ret == 0 && flink.name != 0);

	// One buffer has one name, which is how user space tells whether two
	// handles are the same buffer.
	struct drm_gem_flink again = { .handle = create.handle };

	TEST_RES(ioctl(card_fd, DRM_IOCTL_GEM_FLINK, &again),
		 _ret == 0 && again.name == flink.name);

	struct drm_gem_open opened = { .name = flink.name };

	TEST_RES(ioctl(card_fd, DRM_IOCTL_GEM_OPEN, &opened),
		 _ret == 0 && opened.handle != 0 &&
			 opened.handle != create.handle &&
			 opened.size == create.size);

	// The same buffer under two handles: both map at one offset.
	uint64_t original = 0, imported = 0;

	TEST_SUCC(map_dumb(card_fd, create.handle, &original));
	TEST_SUCC(map_dumb(card_fd, opened.handle, &imported));
	TEST_RES(imported, _ret == original);

	// Closing one handle leaves the buffer reachable through the other,
	// which is the reference counting rule the whole interface rests on.
	struct drm_gem_close close_original = { .handle = create.handle };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_GEM_CLOSE, &close_original));
	TEST_SUCC(map_dumb(card_fd, opened.handle, &imported));
	TEST_RES(imported, _ret == original);

	struct drm_gem_close close_imported = { .handle = opened.handle };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_GEM_CLOSE, &close_imported));

	// An unknown name is ENOENT, as is the name of a buffer whose last
	// handle has just gone away.
	struct drm_gem_open gone = { .name = flink.name };

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_GEM_OPEN, &gone), ENOENT);
}
END_TEST()

/*
 * Exporting a dumb buffer as a dma-buf and importing it must preserve the
 * same backing memory. PRIME sharing lets the GPU and display drivers use
 * the same frame without a copy.
 *
 * The three identities checked here are the ones user space compares:
 * importing one descriptor twice gives one handle, importing into the file that
 * exported it gives back the handle it already had, and a second file's handle
 * resolves to the same fake mmap offset because it is the same object.
 */
FN_TEST(prime_export_and_import_share_one_buffer)
{
	struct drm_mode_create_dumb create = { .width = DUMB_WIDTH,
					       .height = DUMB_HEIGHT,
					       .bpp = DUMB_BPP };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_CREATE_DUMB, &create));

	size_t size = (size_t)create.size;
	uint64_t offset = 0;

	TEST_SUCC(map_dumb(card_fd, create.handle, &offset));

	unsigned char *mem = TEST_RES(mmap(NULL, size, PROT_READ | PROT_WRITE,
					   MAP_SHARED, card_fd, (off_t)offset),
				      _ret != MAP_FAILED);
	SKIP_TEST_IF(mem == MAP_FAILED);

	fill_pattern(mem, size);

	struct drm_prime_handle export = { .handle = create.handle,
					   .flags = DRM_CLOEXEC | DRM_RDWR,
					   .fd = -1 };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &export));
	TEST_RES(export.fd, _ret >= 0);

	// Importing into the file that exported it returns the handle that file
	// already holds, rather than a second one for the same buffer.
	struct drm_prime_handle back = { .fd = export.fd };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &back));
	TEST_RES(back.handle, _ret == create.handle);

	struct drm_prime_handle again = { .fd = export.fd };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &again));
	TEST_RES(again.handle, _ret == back.handle);

	// flags is documented as unused for the import direction.
	struct drm_prime_handle bad_flags = { .fd = export.fd,
					      .flags = DRM_CLOEXEC };

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &bad_flags),
		   EINVAL);

	// A second open file gets a handle of its own for the same object.
	int second = TEST_SUCC(open(CARD_DEVICE, O_RDWR));
	struct drm_prime_handle elsewhere = { .fd = export.fd };

	TEST_SUCC(ioctl(second, DRM_IOCTL_PRIME_FD_TO_HANDLE, &elsewhere));
	TEST_RES(elsewhere.handle, _ret != 0);

	uint64_t other_offset = 0;

	// One object has one fake offset for its whole life, whichever file
	// asks, because the offset space is device-wide.
	TEST_SUCC(map_dumb(second, elsewhere.handle, &other_offset));
	TEST_RES(other_offset, _ret == offset);

	unsigned char *other =
		TEST_RES(mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED,
			      second, (off_t)other_offset),
			 _ret != MAP_FAILED);
	SKIP_TEST_IF(other == MAP_FAILED);

	// Everything written through the exporter's mapping is there.
	TEST_RES(check_pattern(other, size), _ret == 0);

	// And it is the same memory rather than a copy taken at export time: a
	// store through one mapping is visible through the other.
	mem[0] = 0x5a;
	TEST_RES(other[0], _ret == 0x5a);
	other[PAGE_SIZE - 1] = 0x3c;
	TEST_RES(mem[PAGE_SIZE - 1], _ret == 0x3c);

	// The descriptor keeps the buffer alive on its own: closing every
	// handle the exporter holds is what user space does immediately after
	// exporting.
	struct drm_mode_destroy_dumb destroy = { .handle = create.handle };

	TEST_SUCC(ioctl(card_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy));
	TEST_RES(other[0], _ret == 0x5a);
	TEST_RES(other[PAGE_SIZE - 1], _ret == 0x3c);

	TEST_SUCC(munmap(other, size));
	TEST_SUCC(munmap(mem, size));

	struct drm_gem_close close_imported = { .handle = elsewhere.handle };

	TEST_SUCC(ioctl(second, DRM_IOCTL_GEM_CLOSE, &close_imported));
	TEST_SUCC(close(second));
	TEST_SUCC(close(export.fd));
}
END_TEST()

/*
 * A descriptor that is not a dma-buf, and a handle nobody holds.
 */
FN_TEST(prime_rejects_what_it_cannot_share)
{
	struct drm_prime_handle missing = { .handle = 0xdead, .fd = -1 };

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &missing),
		   ENOENT);

	// The DRM node itself is open but is not a shared buffer. dma_buf_get
	// reports EINVAL for that, not ENOTTY.
	struct drm_prime_handle not_a_buffer = { .fd = card_fd };

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &not_a_buffer),
		   EINVAL);

	struct drm_prime_handle closed = { .fd = 0x7fff };

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &closed),
		   EINVAL);
}
END_TEST()

/*
 * PRIME is DRM_RENDER_ALLOW in both directions, which is the whole point of it:
 * a render client that may not touch the display still has to be able to hand
 * it a finished frame.
 */
FN_TEST(prime_is_allowed_on_the_render_node)
{
	struct drm_prime_handle missing = { .handle = 0xdead, .fd = -1 };

	// Reached the handler and failed there, rather than being refused by
	// the node check, which would be EACCES.
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &missing),
		   ENOENT);
}
END_TEST()

/*
 * Sync objects: all eleven ioctls, on the render node.
 *
 * The software driver claims DRIVER_SYNCOBJ and DRIVER_SYNCOBJ_TIMELINE
 * despite having no hardware behind it, because nothing about a sync object is
 * driver-backed -- the handle table and the fences are core code. Claiming
 * them is what lets these tests reach the real path, command decode and
 * user-space copies included, which a kernel-side unit test calling the
 * handlers directly cannot.
 *
 * Every wait below passes an absolute deadline of zero, which is a time in the
 * past, so it is answered from the fences' current state instead of sleeping.
 * A wait that should fail therefore fails at once rather than hanging the
 * suite for its timeout.
 */
static int make_syncobj(int fd, uint32_t flags, uint32_t *handle)
{
	struct drm_syncobj_create arg = { .flags = flags };

	if (ioctl(fd, DRM_IOCTL_SYNCOBJ_CREATE, &arg) < 0)
		return -1;

	*handle = arg.handle;
	return 0;
}

static int drop_syncobj(int fd, uint32_t handle)
{
	struct drm_syncobj_destroy arg = { .handle = handle };

	return ioctl(fd, DRM_IOCTL_SYNCOBJ_DESTROY, &arg);
}

static int wait_syncobj(int fd, uint32_t handle, uint32_t flags)
{
	uint32_t handles[1] = { handle };
	struct drm_syncobj_wait arg = { .handles = (uint64_t)(uintptr_t)handles,
					.count_handles = 1,
					.flags = flags };

	return ioctl(fd, DRM_IOCTL_SYNCOBJ_WAIT, &arg);
}

static int wait_point(int fd, uint32_t handle, uint64_t point, uint32_t flags)
{
	uint32_t handles[1] = { handle };
	uint64_t points[1] = { point };
	struct drm_syncobj_timeline_wait arg = {
		.handles = (uint64_t)(uintptr_t)handles,
		.points = (uint64_t)(uintptr_t)points,
		.count_handles = 1,
		.flags = flags
	};

	return ioctl(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &arg);
}

static int signal_syncobj(int fd, uint32_t handle)
{
	uint32_t handles[1] = { handle };
	struct drm_syncobj_array arg = {
		.handles = (uint64_t)(uintptr_t)handles, .count_handles = 1
	};

	return ioctl(fd, DRM_IOCTL_SYNCOBJ_SIGNAL, &arg);
}

static int reset_syncobj(int fd, uint32_t handle)
{
	uint32_t handles[1] = { handle };
	struct drm_syncobj_array arg = {
		.handles = (uint64_t)(uintptr_t)handles, .count_handles = 1
	};

	return ioctl(fd, DRM_IOCTL_SYNCOBJ_RESET, &arg);
}

static int signal_point(int fd, uint32_t handle, uint64_t point)
{
	uint32_t handles[1] = { handle };
	uint64_t points[1] = { point };
	struct drm_syncobj_timeline_array arg = {
		.handles = (uint64_t)(uintptr_t)handles,
		.points = (uint64_t)(uintptr_t)points,
		.count_handles = 1
	};

	return ioctl(fd, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &arg);
}

static int query_timeline(int fd, uint32_t handle, uint32_t flags,
			  uint64_t *value)
{
	uint32_t handles[1] = { handle };
	uint64_t points[1] = { 0 };
	struct drm_syncobj_timeline_array arg = {
		.handles = (uint64_t)(uintptr_t)handles,
		.points = (uint64_t)(uintptr_t)points,
		.count_handles = 1,
		.flags = flags
	};

	if (ioctl(fd, DRM_IOCTL_SYNCOBJ_QUERY, &arg) < 0)
		return -1;

	*value = points[0];
	return 0;
}

/*
 * A sync object is born either empty or already signalled, and a wait can tell
 * the two apart.
 */
FN_TEST(a_sync_object_is_created_empty_or_signalled)
{
	uint32_t empty = 0, signalled = 0;

	TEST_SUCC(make_syncobj(render_fd, 0, &empty));
	TEST_SUCC(make_syncobj(render_fd, DRM_SYNCOBJ_CREATE_SIGNALED,
			       &signalled));

	TEST_SUCC(wait_syncobj(render_fd, signalled, 0));

	// An empty object holds no fence at all, which is not the same as
	// holding one that has yet to signal: there is nothing to wait on, and
	// drm_syncobj_find_fence answers EINVAL rather than a timeout.
	TEST_ERRNO(wait_syncobj(render_fd, empty, 0), EINVAL);

	// WAIT_FOR_SUBMIT asks to wait for the fence to turn up instead, which
	// makes this a real wait -- one that then runs out of time. ETIME and
	// not ETIMEDOUT, because vkWaitForFences turns exactly 62 into
	// VK_TIMEOUT.
	TEST_ERRNO(wait_syncobj(render_fd, empty,
				DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT),
		   ETIME);

	TEST_SUCC(drop_syncobj(render_fd, empty));
	TEST_SUCC(drop_syncobj(render_fd, signalled));

	// Destroying releases the handle rather than leaving a stale entry.
	TEST_ERRNO(drop_syncobj(render_fd, empty), ENOENT);
}
END_TEST()

/*
 * Signalling and resetting are the two CPU-side ways to move a binary sync
 * object, and a later wait has to see both.
 */
FN_TEST(signalling_and_resetting_a_sync_object_round_trip)
{
	uint32_t handle = 0;

	TEST_SUCC(make_syncobj(render_fd, 0, &handle));
	TEST_ERRNO(wait_syncobj(render_fd, handle, 0), EINVAL);

	TEST_SUCC(signal_syncobj(render_fd, handle));
	TEST_SUCC(wait_syncobj(render_fd, handle, 0));

	// Reset empties the object rather than clearing a flag on it, so the
	// wait is back to having no fence to look at, EINVAL and not a timeout.
	TEST_SUCC(reset_syncobj(render_fd, handle));
	TEST_ERRNO(wait_syncobj(render_fd, handle, 0), EINVAL);

	TEST_SUCC(drop_syncobj(render_fd, handle));
}
END_TEST()

/*
 * A sync object reaches another client as a file descriptor. What the importer
 * gets has to be the same object, not a copy of its current state: two clients
 * sharing one is the entire point.
 *
 * Two separate opens of the render node, because handles are per-file. Within
 * one file the import is deduplicated back to the handle already held, which
 * would make "signal through one, observe through the other" prove nothing.
 */
FN_TEST(a_sync_object_shared_by_descriptor_is_one_object)
{
	struct drm_syncobj_handle export = { .fd = -1 };
	struct drm_syncobj_handle import = { .fd = -1 };
	uint32_t owner = 0;
	int peer_fd = -1;

	TEST_RES(peer_fd = open(RENDER_DEVICE, O_RDWR), _ret >= 0);

	TEST_SUCC(make_syncobj(render_fd, 0, &owner));

	export.handle = owner;
	TEST_RES(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &export),
		 export.fd >= 0);

	import.fd = export.fd;
	TEST_RES(ioctl(peer_fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &import),
		 import.handle != 0);

	// Neither client can wait yet: an object with no fence is not an
	// unsignalled one.
	TEST_ERRNO(wait_syncobj(peer_fd, import.handle, 0), EINVAL);

	// Signalled by its owner, seen by the importer. That is the property a
	// copy would not have.
	TEST_SUCC(signal_syncobj(render_fd, owner));
	TEST_SUCC(wait_syncobj(peer_fd, import.handle, 0));

	// The descriptor keeps the object alive on its own, so the importer's
	// handle still works after the owner has let go of both.
	TEST_SUCC(drop_syncobj(render_fd, owner));
	TEST_SUCC(close(export.fd));
	TEST_SUCC(wait_syncobj(peer_fd, import.handle, 0));

	TEST_SUCC(drop_syncobj(peer_fd, import.handle));
	TEST_SUCC(close(peer_fd));
}
END_TEST()

/*
 * A timeline signals points in increasing order, and reports how far it has
 * got. Vulkan timeline semaphores are this and nothing more.
 */
FN_TEST(a_timeline_reports_the_point_it_has_reached)
{
	uint32_t handle = 0;
	uint64_t value = 0;

	TEST_SUCC(make_syncobj(render_fd, 0, &handle));

	TEST_RES(query_timeline(render_fd, handle, 0, &value), value == 0);

	TEST_SUCC(signal_point(render_fd, handle, 5));
	TEST_RES(query_timeline(render_fd, handle, 0, &value), value == 5);

	// Signalling from the CPU submits and completes in one step, so the
	// last submitted point and the last signalled one agree here. They come
	// apart once a GPU job is what advances the timeline.
	TEST_RES(query_timeline(render_fd, handle,
				DRM_SYNCOBJ_QUERY_FLAGS_LAST_SUBMITTED, &value),
		 value == 5);

	// A point already reached is satisfied, and so is an earlier one:
	// points signal in order, so a later one implies every earlier one.
	TEST_SUCC(wait_point(render_fd, handle, 5, 0));
	TEST_SUCC(wait_point(render_fd, handle, 3, 0));

	// Nothing has been submitted for point 6, which is not the same as work
	// that has not finished.
	TEST_ERRNO(wait_point(render_fd, handle, 6, 0), EINVAL);

	// This port admits only nonzero points above its last submitted point.
	// Linux may accept already completed points as no-ops; this checks the
	// port's explicit admission rule, not general Linux timeline semantics.
	TEST_ERRNO(signal_point(render_fd, handle, 5), EINVAL);
	TEST_ERRNO(signal_point(render_fd, handle, 0), EINVAL);
	TEST_SUCC(signal_point(render_fd, handle, 6));

	TEST_SUCC(drop_syncobj(render_fd, handle));
}
END_TEST()

/*
 * Transfer copies a fence between sync objects. The destination point decides
 * which kind of copy it is: zero replaces the binary fence, non-zero adds a
 * timeline point.
 */
FN_TEST(a_fence_can_be_transferred_between_sync_objects)
{
	struct drm_syncobj_transfer transfer = { 0 };
	uint32_t source = 0, binary = 0, timeline = 0;
	uint64_t value = 0;

	TEST_SUCC(
		make_syncobj(render_fd, DRM_SYNCOBJ_CREATE_SIGNALED, &source));
	TEST_SUCC(make_syncobj(render_fd, 0, &binary));
	TEST_SUCC(make_syncobj(render_fd, 0, &timeline));

	// Binary to binary: the destination ends up holding the source's fence,
	// which is already signalled.
	transfer.src_handle = source;
	transfer.dst_handle = binary;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_TRANSFER, &transfer));
	TEST_SUCC(wait_syncobj(render_fd, binary, 0));

	// Binary to a timeline point: the same fence, now reachable by point.
	transfer.dst_handle = timeline;
	transfer.dst_point = 7;
	TEST_SUCC(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_TRANSFER, &transfer));
	TEST_SUCC(wait_point(render_fd, timeline, 7, 0));
	TEST_RES(query_timeline(render_fd, timeline, 0, &value), value == 7);

	// An empty source has no fence to copy, and says so rather than
	// quietly leaving the destination as it was.
	transfer.src_handle = binary;
	transfer.dst_handle = source;
	transfer.dst_point = 0;
	TEST_SUCC(reset_syncobj(render_fd, binary));
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_SYNCOBJ_TRANSFER, &transfer),
		   EINVAL);

	TEST_SUCC(drop_syncobj(render_fd, source));
	TEST_SUCC(drop_syncobj(render_fd, binary));
	TEST_SUCC(drop_syncobj(render_fd, timeline));
}
END_TEST()

/*
 * Dumb buffers are scanout buffers, so their ioctls carry no DRM_RENDER_ALLOW
 * and neither does flink, which is a device-global namespace. GEM_CLOSE does,
 * because a render client allocates buffers and has to be able to free them.
 */
FN_TEST(render_node_refuses_dumb_buffer_ioctls)
{
	struct drm_mode_create_dumb create = { .width = 16,
					       .height = 16,
					       .bpp = 32 };
	struct drm_mode_map_dumb map = { .handle = 1 };
	struct drm_mode_destroy_dumb destroy = { .handle = 1 };
	struct drm_gem_flink flink = { .handle = 1 };
	struct drm_gem_open open_by_name = { .name = 1 };
	struct drm_gem_close close = { .handle = 0xdead };

	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_MODE_CREATE_DUMB, &create),
		   EACCES);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_MODE_MAP_DUMB, &map), EACCES);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_MODE_DESTROY_DUMB, &destroy),
		   EACCES);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_GEM_FLINK, &flink), EACCES);
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_GEM_OPEN, &open_by_name), EACCES);

	// Allowed through, and failing only because the handle is not there.
	TEST_ERRNO(ioctl(render_fd, DRM_IOCTL_GEM_CLOSE, &close), EINVAL);
}
END_TEST()

/*
 * Linux distinguishes an unimplemented DRM command from something that is not a
 * DRM command at all, and libdrm probes for optional interfaces by the
 * difference.
 */
FN_TEST(unimplemented_commands)
{
	int busid[4] = { 0 };

	TEST_ERRNO(ioctl(card_fd, DRM_IOCTL_IRQ_BUSID, busid), EINVAL);
	TEST_ERRNO(ioctl(card_fd, TIOCGWINSZ, busid), ENOTTY);
}
END_TEST()

FN_TEST(write_is_refused)
{
	char byte = 0;

	// DRM nodes deliver events through read and have no write handler, in
	// Linux either. Reading is deliberately not exercised here: it will block
	// while no event is queued.
	TEST_ERRNO(write(card_fd, &byte, sizeof(byte)), EINVAL);
}
END_TEST()

FN_SETUP(close_drm_nodes)
{
	CHECK(close(render_fd));
	CHECK(close(card_fd));
}
END_SETUP()
