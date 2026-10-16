// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: user-space regression for demand-paged GEM device mappings.

/*
 * Demand paging of device file mappings, from user space.
 *
 * A device file may register a fault handler instead of an MMIO window, in
 * which case `mmap` maps nothing and each page is provided by the device the
 * first time it is touched. This test checks the three properties that make
 * such a mapping usable for a GPU buffer object:
 *
 *   1. mapping a large range costs nothing until it is touched;
 *   2. touching a scattered subset of pages backs exactly those pages;
 *   3. what the CPU wrote is what the CPU reads back, per page, and the pages
 *      are visible through a second concurrent mapping of the same object.
 *
 * Which device to use is not hardcoded, because the first in-tree device with
 * a demand-paged mapping happens to be a DRM node, and nothing here should be
 * specific to it. Point this test at one with:
 *
 *     ON_DEMAND_DEVICE=/dev/...     the device file to mmap (required)
 *     ON_DEMAND_LENGTH=<bytes>      how much to map (default 1 MiB)
 *     ON_DEMAND_OFFSET=<bytes>      the mmap offset, page aligned (default 0)
 *     ON_DEMAND_GEM_NAME=<name>     a DRM flink name to claim the buffer with,
 *                                   which then supplies the offset instead
 *
 * Without ON_DEMAND_DEVICE, or if the device is absent, every test reports a
 * skip and the program succeeds. That matches how `device/framebuffer.c`
 * handles a missing `/dev/fb0`.
 *
 * `device/drm/drm_dumb_hold` sets all of this up; see the comment there.
 */

#define _GNU_SOURCE

#include "../../common/test.h"

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

#define PAGE_SIZE 4096
#define DEFAULT_LENGTH (1024 * 1024)
/* How many of the mapped pages to touch. The point is that this is far fewer
 * than the number of mapped pages. */
#define NUM_TOUCHED 24

static const char *device_path;
static size_t device_offset;
static size_t map_length;
static size_t num_pages;

/* The one piece of device-specific knowledge in this test, and it is here
 * rather than in the wrapper that creates the buffer because it cannot be
 * anywhere else: a DRM node only lets a file map a buffer object that *that
 * file* holds a handle to, and a handle belongs to an open file, so it cannot
 * be inherited. This test opens the device itself, so it has to ask for a
 * handle itself. ON_DEMAND_GEM_NAME is the buffer's flink name; GEM_OPEN turns
 * it into a handle, and MODE_MAP_DUMB turns the handle into the offset to map.
 *
 * Zero means the device needs none of this, and ON_DEMAND_OFFSET is used as it
 * is. */
static uint32_t gem_name;

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L795>. */
struct drm_gem_open {
	uint32_t name;
	uint32_t handle;
	uint64_t size;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm_mode.h#L953>. */
struct drm_mode_map_dumb {
	uint32_t handle;
	uint32_t pad;
	uint64_t offset;
};

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L993>. */
#define DRM_IOCTL_BASE 'd'
#define DRM_IOCTL_GEM_OPEN _IOWR(DRM_IOCTL_BASE, 0x0b, struct drm_gem_open)
#define DRM_IOCTL_MODE_MAP_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB3, struct drm_mode_map_dumb)

/* The pages to touch, drawn from the whole mapping with a fixed seed so that a
 * failure can be reproduced. */
static size_t touched[NUM_TOUCHED];

static size_t env_size(const char *name, size_t fallback, uint64_t maximum)
{
	const char *value = getenv(name);
	if (value == NULL || *value == '\0')
		return fallback;

	char *end = NULL;
	errno = 0;
	unsigned long long parsed = strtoull(value, &end, 0);
	if (*value < '0' || *value > '9' || errno == ERANGE || end == value ||
	    *end != '\0' || parsed > maximum) {
		fprintf(stderr,
			"fatal error: %s is not an unsigned value in range: %s\n",
			name, value);
		exit(EXIT_FAILURE);
	}
	return (size_t)parsed;
}

/* A tiny linear congruential generator, so that the page order does not depend
 * on the libc's rand(). */
static uint32_t next_random(uint32_t *state)
{
	*state = *state * 1103515245u + 12345u;
	return *state >> 16;
}

static int is_already_touched(size_t page, size_t count)
{
	for (size_t i = 0; i < count; ++i) {
		if (touched[i] == page)
			return 1;
	}
	return 0;
}

/* The byte a given page of the mapping should hold, once written. */
static unsigned char pattern_of(size_t page)
{
	return (unsigned char)(0x40 + (page % 173));
}

static void write_page(void *base, size_t page)
{
	volatile unsigned char *p = (unsigned char *)base + page * PAGE_SIZE;

	p[0] = pattern_of(page);
	p[PAGE_SIZE / 2] = pattern_of(page);
	p[PAGE_SIZE - 1] = pattern_of(page);
}

static int page_reads_back(void *base, size_t page)
{
	volatile unsigned char *p = (unsigned char *)base + page * PAGE_SIZE;
	unsigned char expected = pattern_of(page);

	return p[0] == expected && p[PAGE_SIZE / 2] == expected &&
	       p[PAGE_SIZE - 1] == expected;
}

/* Returns how many of the touched pages do not read back what was written,
 * naming each one so that a failure points at a page rather than a count. */
static int count_mismatches(void *base)
{
	int mismatches = 0;

	for (size_t i = 0; i < NUM_TOUCHED; ++i) {
		if (page_reads_back(base, touched[i]))
			continue;
		fprintf(stderr, "page %zu of the mapping does not read back\n",
			touched[i]);
		mismatches++;
	}
	return mismatches;
}

static long rss_file_kb(void)
{
	FILE *f = fopen("/proc/self/status", "r");
	if (f == NULL) {
		perror("fopen /proc/self/status");
		exit(EXIT_FAILURE);
	}

	char line[256];
	long rss_kb = -1;
	while (fgets(line, sizeof(line), f)) {
		if (strncmp(line, "RssFile:", 8) == 0) {
			sscanf(line + 8, "%ld", &rss_kb);
			break;
		}
	}
	fclose(f);

	if (rss_kb < 0) {
		fprintf(stderr,
			"fatal error: no RssFile in /proc/self/status\n");
		exit(EXIT_FAILURE);
	}
	return rss_kb;
}

static long pages_to_kb(size_t pages)
{
	return (long)(pages * (PAGE_SIZE / 1024));
}

FN_SETUP(configure)
{
	device_path = getenv("ON_DEMAND_DEVICE");
	device_offset = env_size("ON_DEMAND_OFFSET", 0, INT64_MAX);
	map_length = env_size("ON_DEMAND_LENGTH", DEFAULT_LENGTH, INT64_MAX);
	gem_name = (uint32_t)env_size("ON_DEMAND_GEM_NAME", 0, UINT32_MAX);

	if (device_offset % PAGE_SIZE != 0 || map_length % PAGE_SIZE != 0) {
		fprintf(stderr,
			"fatal error: the offset and the length must be page aligned\n");
		exit(EXIT_FAILURE);
	}

	num_pages = map_length / PAGE_SIZE;
	if (num_pages < NUM_TOUCHED * 2) {
		fprintf(stderr,
			"fatal error: the mapping must be much larger than the %d touched pages\n",
			NUM_TOUCHED);
		exit(EXIT_FAILURE);
	}

	uint32_t state = 0x5eed1234u;
	for (size_t i = 0; i < NUM_TOUCHED; ++i) {
		size_t page;
		do {
			page = next_random(&state) % num_pages;
		} while (is_already_touched(page, i));
		touched[i] = page;
	}

	if (device_path == NULL) {
		fprintf(stderr,
			"note: ON_DEMAND_DEVICE is unset, skipping every test\n");
	}
}
END_SETUP()

/* Claims the buffer named by ON_DEMAND_GEM_NAME for this file and takes the
 * offset to map from it.
 *
 * Unlike a missing device, a failure here is fatal: the name is only set once
 * the buffer has been created and published, so being unable to claim it is a
 * kernel bug rather than an absent prerequisite. Every file gets the same
 * offset for the same buffer, so repeating this per open is not a problem. */
static void claim_gem_buffer(int fd)
{
	struct drm_gem_open opened = { .name = gem_name };

	if (ioctl(fd, DRM_IOCTL_GEM_OPEN, &opened) < 0) {
		fprintf(stderr,
			"fatal error: cannot claim GEM name %u on %s: %s\n",
			gem_name, device_path, strerror(errno));
		exit(EXIT_FAILURE);
	}

	struct drm_mode_map_dumb map = { .handle = opened.handle };

	if (ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &map) < 0) {
		fprintf(stderr,
			"fatal error: cannot get an offset for handle %u: %s\n",
			opened.handle, strerror(errno));
		exit(EXIT_FAILURE);
	}

	if (map.offset % PAGE_SIZE != 0) {
		fprintf(stderr,
			"fatal error: the offset %llu is not page aligned\n",
			(unsigned long long)map.offset);
		exit(EXIT_FAILURE);
	}

	/* Mapping past the end of the buffer would fault on pages that have no
	 * backing store, which is a different test. */
	if (opened.size < map_length) {
		fprintf(stderr,
			"fatal error: the buffer is %llu bytes, smaller than the %zu mapped\n",
			(unsigned long long)opened.size, map_length);
		exit(EXIT_FAILURE);
	}

	device_offset = (size_t)map.offset;
}

/* Opens the device, or returns -1 if it is not there. Not a setup function:
 * a missing device is a skip, not a fatal error. */
static int open_device(void)
{
	if (device_path == NULL)
		return -1;

	int fd = open(device_path, O_RDWR);
	if (fd < 0) {
		int error = errno;
		if (error == ENOENT || error == ENODEV) {
			fprintf(stderr, "note: device absent %s: %s\n",
				device_path, strerror(error));
			errno = 0;
			return -1;
		}
		fprintf(stderr, "fatal error: cannot open %s: %s\n",
			device_path, strerror(error));
		exit(EXIT_FAILURE);
	}

	if (gem_name != 0)
		claim_gem_buffer(fd);
	return fd;
}

FN_TEST(mapping_alone_backs_nothing)
{
	int fd = open_device();
	SKIP_TEST_IF(fd < 0);

	long rss_before = rss_file_kb();

	void *mem = CHECK_WITH(mmap(NULL, map_length, PROT_READ | PROT_WRITE,
				    MAP_SHARED, fd, device_offset),
			       _ret != MAP_FAILED);

	/* The whole point of demand paging: reserving the address space must
	 * not bring in a single page. */
	TEST_RES(rss_file_kb(), _ret == rss_before);

	TEST_SUCC(munmap(mem, map_length));
	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(scattered_access_backs_only_the_touched_pages)
{
	int fd = open_device();
	SKIP_TEST_IF(fd < 0);

	void *mem = CHECK_WITH(mmap(NULL, map_length, PROT_READ | PROT_WRITE,
				    MAP_SHARED, fd, device_offset),
			       _ret != MAP_FAILED);

	long rss_before = rss_file_kb();

	for (size_t i = 0; i < NUM_TOUCHED; ++i)
		write_page(mem, touched[i]);

	TEST_RES(rss_file_kb(), _ret == rss_before + pages_to_kb(NUM_TOUCHED));

	/* Touching the same pages again resolves no new page. */
	for (size_t i = 0; i < NUM_TOUCHED; ++i)
		write_page(mem, touched[i]);

	TEST_RES(rss_file_kb(), _ret == rss_before + pages_to_kb(NUM_TOUCHED));

	TEST_SUCC(munmap(mem, map_length));

	/* Unmapping drops the mapping's reference to every page it resolved. */
	TEST_RES(rss_file_kb(), _ret == rss_before);

	TEST_SUCC(close(fd));
}
END_TEST()

FN_TEST(writes_read_back_per_page)
{
	int fd = open_device();
	SKIP_TEST_IF(fd < 0);

	void *mem = CHECK_WITH(mmap(NULL, map_length, PROT_READ | PROT_WRITE,
				    MAP_SHARED, fd, device_offset),
			       _ret != MAP_FAILED);

	for (size_t i = 0; i < NUM_TOUCHED; ++i)
		write_page(mem, touched[i]);

	TEST_RES(count_mismatches(mem), _ret == 0);

	/* The pages belong to the object, not to this mapping, so a second
	 * mapping of the same offsets sees the same bytes. */
	void *again = CHECK_WITH(mmap(NULL, map_length, PROT_READ | PROT_WRITE,
				      MAP_SHARED, fd, device_offset),
				 _ret != MAP_FAILED);

	TEST_RES(count_mismatches(again), _ret == 0);

	TEST_SUCC(munmap(again, map_length));
	TEST_SUCC(munmap(mem, map_length));
	TEST_SUCC(close(fd));
}
END_TEST()
