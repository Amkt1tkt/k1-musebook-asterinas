// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: keep a dumb buffer alive across a mapping-test exec.

/*
 * Creates a dumb buffer, publishes it, then runs a command that can map it.
 *
 *     drm_dumb_hold <program> [args...]
 *
 * This exists for `memory/mmap_device/mmap_on_demand_device`, which needs a
 * device file whose mapping is demand-paged. The first such device in the tree
 * is a DRM node, and the thing to map is a GEM buffer object, which has to be
 * created by an ioctl and stays alive only while somebody holds a handle to it.
 * So this program:
 *
 *   1. creates a dumb buffer on /dev/dri/card0 and keeps its fd open across the
 *      exec, which is what keeps the handle — and therefore the buffer — alive
 *      for as long as the command runs;
 *   2. publishes it under a flink name and exports that name, so that the
 *      command can call GEM_OPEN and get a handle of its own. Only a file
 *      holding a handle may map the buffer, and a handle belongs to an open
 *      file, so it cannot be handed over — the command opens the node itself
 *      and has to ask for its own;
 *   3. drops mastership, because GEM_OPEN carries DRM_AUTH; this wrapper does not
 *      perform an AUTH_MAGIC handshake with the command. The first
 *      client to open a primary node that has no master becomes master, and is
 *      authenticated by that; holding on to it here would leave the command
 *      unauthenticated unless it happened to be running as root;
 *   4. exports ON_DEMAND_DEVICE, ON_DEMAND_GEM_NAME and ON_DEMAND_LENGTH, and
 *      execs the command.
 *
 * If there is no DRM node, the command is run anyway with the variables unset,
 * which is the case those tests skip themselves in. That keeps this wrapper
 * safe to put in front of a test unconditionally.
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define CARD_DEVICE "/dev/dri/card0"

/* One mebibyte: comfortably more pages than the test scatters its writes over,
 * so that "only the touched pages are mapped into the process" is actually being measured. */
#define BUFFER_WIDTH 1024
#define BUFFER_HEIGHT 256
#define BUFFER_BPP 32

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L789>. */
struct drm_gem_flink {
	uint32_t handle;
	uint32_t name;
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

/* Reference: <https://elixir.bootlin.com/linux/v6.6/source/include/uapi/drm/drm.h#L993>. */
#define DRM_IOCTL_BASE 'd'
#define DRM_IOCTL_GEM_FLINK _IOWR(DRM_IOCTL_BASE, 0x0a, struct drm_gem_flink)
#define DRM_IOCTL_DROP_MASTER _IO(DRM_IOCTL_BASE, 0x1f)
#define DRM_IOCTL_MODE_CREATE_DUMB \
	_IOWR(DRM_IOCTL_BASE, 0xB2, struct drm_mode_create_dumb)

/* Returns 0 when prepared, 1 for an absent optional device, and -1 on error. */
static int prepare(void)
{
	int fd = open(CARD_DEVICE, O_RDWR);
	if (fd < 0) {
		int error = errno;
		fprintf(stderr, "cannot open %s: %s\n", CARD_DEVICE,
			strerror(error));
		return error == ENOENT || error == ENODEV || error == ENXIO ?
			       1 :
			       -1;
	}

	struct drm_mode_create_dumb create = {
		.width = BUFFER_WIDTH,
		.height = BUFFER_HEIGHT,
		.bpp = BUFFER_BPP,
	};

	if (ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &create) < 0) {
		fprintf(stderr, "note: cannot create a dumb buffer: %s\n",
			strerror(errno));
		close(fd);
		return -1;
	}

	struct drm_gem_flink flink = { .handle = create.handle };

	if (ioctl(fd, DRM_IOCTL_GEM_FLINK, &flink) < 0) {
		fprintf(stderr, "note: cannot publish the dumb buffer: %s\n",
			strerror(errno));
		close(fd);
		return -1;
	}

	/* Hand mastership back so that the command's own open of the node
	 * acquires it, and with it the authentication GEM_OPEN requires. A
	 * failure here is not fatal: it means this process never held it, and
	 * the command may still be authenticated by being root. */
	if (ioctl(fd, DRM_IOCTL_DROP_MASTER, 0) < 0) {
		fprintf(stderr, "note: cannot drop mastership: %s\n",
			strerror(errno));
		errno = 0;
	}

	char name[32], length[32];

	snprintf(name, sizeof(name), "%u", flink.name);
	snprintf(length, sizeof(length), "%llu",
		 (unsigned long long)create.size);

	if (setenv("ON_DEMAND_DEVICE", CARD_DEVICE, 1) != 0 ||
	    setenv("ON_DEMAND_GEM_NAME", name, 1) != 0 ||
	    setenv("ON_DEMAND_LENGTH", length, 1) != 0) {
		perror("setenv");
		close(fd);
		return -1;
	}

	fprintf(stderr,
		"holding a %llu-byte dumb buffer (handle %u) under name %s\n",
		(unsigned long long)create.size, create.handle, name);

	/* Deliberately left open: closing it would release the last handle and
	 * with it the buffer. It is not CLOEXEC, so the command inherits it. */
	return 0;
}

int main(int argc, char *argv[])
{
	if (argc < 2) {
		fprintf(stderr, "usage: %s <program> [args...]\n", argv[0]);
		return EXIT_FAILURE;
	}

	/* This wrapper owns these variables; never inherit a stale mapping. */
	unsetenv("ON_DEMAND_DEVICE");
	unsetenv("ON_DEMAND_GEM_NAME");
	unsetenv("ON_DEMAND_LENGTH");
	if (prepare() < 0)
		return EXIT_FAILURE;

	execv(argv[1], &argv[1]);

	fprintf(stderr, "fatal error: cannot run %s: %s\n", argv[1],
		strerror(errno));
	return EXIT_FAILURE;
}
