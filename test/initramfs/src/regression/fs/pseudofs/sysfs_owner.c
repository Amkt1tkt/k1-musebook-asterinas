// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: sysfs lookup must not grant ownership of kernel-created nodes.

#define _GNU_SOURCE

#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include "../../common/test.h"

static char mountpoint[] = "/tmp/sysfs-owner-XXXXXX";

FN_SETUP(prepare)
{
	CHECK(unshare(CLONE_NEWNS));
	CHECK(mount(NULL, "/", NULL, MS_PRIVATE | MS_REC, NULL));
	CHECK_WITH(mkdtemp(mountpoint), _ret != NULL);
	// A new mount has a fresh dentry cache even if /sys was already read.
	CHECK(mount("sysfs", mountpoint, "sysfs", 0, NULL));
}
END_SETUP()

FN_TEST(unprivileged_lookup_preserves_kernel_ownership)
{
	pid_t child = TEST_SUCC(fork());
	if (child == 0) {
		CHECK(chdir(mountpoint));
		CHECK(setgroups(0, NULL));
		CHECK(setgid(65534));
		CHECK(setuid(65534));

		// The child performs the first lookup below the fresh mount.
		const char *paths[] = {
			"kernel",
			"kernel/cpu_byteorder",
			"devices",
		};
		int failures = 0;
		for (size_t i = 0; i < sizeof(paths) / sizeof(paths[0]); ++i) {
			const char *path = paths[i];
			struct stat st;
			CHECK(stat(path, &st));
			if (st.st_uid != 0 || st.st_gid != 0) {
				fprintf(stderr, "%s: unexpected owner %u:%u\n",
					paths[i], st.st_uid, st.st_gid);
				++failures;
			}

			// Request owner-write without writing to an actual device.
			errno = 0;
			int ret = chmod(path, (st.st_mode & 07777) | S_IWUSR);
			if (ret != -1 || errno != EPERM) {
				fprintf(stderr,
					"%s: chmod returned %d, errno %d\n",
					paths[i], ret, errno);
				++failures;
			}
		}

		errno = 0;
		int fd = open("kernel/cpu_byteorder", O_WRONLY);
		if (fd >= 0 || errno != EACCES) {
			fprintf(stderr,
				"sysfs attribute open returned %d, errno %d\n",
				fd, errno);
			++failures;
			if (fd >= 0)
				CHECK(close(fd));
		}
		_exit(failures ? EXIT_FAILURE : EXIT_SUCCESS);
	}

	int status;
	TEST_RES(waitpid(child, &status, 0),
		 _ret == child && WIFEXITED(status) &&
			 WEXITSTATUS(status) == EXIT_SUCCESS);
}
END_TEST()

FN_SETUP(cleanup)
{
	CHECK(umount(mountpoint));
	CHECK(rmdir(mountpoint));
}
END_SETUP()
