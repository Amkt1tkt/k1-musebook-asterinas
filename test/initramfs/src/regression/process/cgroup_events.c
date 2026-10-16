// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: cgroup.events identity and populated notification regression.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define TIMEOUT_MS 3000
#define QUIET_MS 200
#define ROOT_BIT 1u
#define A_BIT 2u
#define B_BIT 4u
#define CGROUP2_MAGIC 0x63677270

struct group {
	char dir[PATH_MAX];
	char events[PATH_MAX];
	int wd;
	int created;
};
struct child {
	pid_t pid;
	int control;
};
static struct group groups[3];
static struct child children[2] = { { 0, -1 }, { 0, -1 } };
static int notify_fd = -1;
static int epoll_fd = -1;
static const char *phase = "setup";
static volatile sig_atomic_t interrupted;

static void on_signal(int sig)
{
	interrupted = sig;
}

static void begin_phase(const char *next)
{
	phase = next;
	printf("BEGIN phase=%s\n", phase);
}

static int fail(const char *operation)
{
	int err = errno;
	fprintf(stderr, "FAIL phase=%s operation=%s errno=%d (%s)\n", phase,
		operation, err, strerror(err));
	return -1;
}

static int64_t now_ms(void)
{
	struct timespec ts;
	if (clock_gettime(CLOCK_MONOTONIC, &ts) < 0)
		return -1;
	return (int64_t)ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static int make_path(char *dst, size_t size, const char *dir, const char *name)
{
	size_t a = strlen(dir), b = strlen(name);
	if (a + b + 2 > size) {
		errno = ENAMETOOLONG;
		return fail("path length");
	}
	memcpy(dst, dir, a);
	dst[a] = '/';
	memcpy(dst + a + 1, name, b + 1);
	return 0;
}

static int read_text(const char *path, char *buf, size_t size)
{
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0)
		return fail(path);
	size_t used = 0;
	while (used + 1 < size) {
		ssize_t n = read(fd, buf + used, size - used - 1);
		if (n < 0 && errno == EINTR && !interrupted)
			continue;
		if (n < 0) {
			int err = errno;
			close(fd);
			errno = err;
			return fail(path);
		}
		if (n == 0) {
			buf[used] = '\0';
			close(fd);
			return 0;
		}
		used += (size_t)n;
	}
	close(fd);
	errno = EOVERFLOW;
	return fail("read buffer exhausted");
}

static int populated(int index)
{
	char buf[256];
	if (read_text(groups[index].events, buf, sizeof(buf)) < 0)
		return -1;
	for (char *line = buf; line && *line;) {
		unsigned int value;
		if (sscanf(line, "populated %u", &value) == 1 && value <= 1)
			return (int)value;
		line = strchr(line, '\n');
		if (line)
			++line;
	}
	errno = EPROTO;
	return fail("missing populated 0/1");
}

static int expect_state(int root, int a, int b)
{
	const int want[3] = { root, a, b };
	for (int i = 0; i < 3; ++i) {
		int got = populated(i);
		if (got < 0)
			return -1;
		if (got != want[i]) {
			fprintf(stderr, "group=%d populated=%d expected=%d\n",
				i, got, want[i]);
			errno = EPROTO;
			return fail("populated state");
		}
	}
	return 0;
}

/* The watch must survive all transient open/read/close references. */
static int check_identity(void)
{
	for (int i = 0; i < 3; ++i) {
		for (int n = 0; n < 32; ++n)
			if (populated(i) < 0)
				return -1;
		int wd = inotify_add_watch(notify_fd, groups[i].events,
					   IN_MODIFY);
		if (wd < 0)
			return fail("repeat inotify_add_watch");
		if (wd != groups[i].wd) {
			fprintf(stderr, "group=%d watch=%d expected=%d\n", i,
				wd, groups[i].wd);
			errno = EPROTO;
			return fail(
				"cgroup.events inode/watch identity changed");
		}
	}
	return 0;
}

static int drain_events(unsigned int *seen, int64_t deadline)
{
	union {
		struct inotify_event align;
		char bytes[4096];
	} buf;
	for (;;) {
		int64_t now = now_ms();
		if (now < 0)
			return fail("clock_gettime");
		if (interrupted || now >= deadline) {
			errno = interrupted ? EINTR : ETIMEDOUT;
			return fail("inotify drain timeout/interruption");
		}
		ssize_t n = read(notify_fd, buf.bytes, sizeof(buf.bytes));
		if (n < 0 && errno == EAGAIN)
			return 0;
		if (n < 0 && errno == EINTR && !interrupted)
			continue;
		if (n <= 0) {
			if (n == 0)
				errno = EIO;
			return fail("inotify read");
		}
		for (size_t off = 0; off < (size_t)n;) {
			if ((size_t)n - off < sizeof(struct inotify_event)) {
				errno = EPROTO;
				return fail("short inotify event");
			}
			struct inotify_event *ev =
				(struct inotify_event *)(buf.bytes + off);
			if (sizeof(*ev) + ev->len > (size_t)n - off) {
				errno = EPROTO;
				return fail("invalid inotify event length");
			}
			if (ev->mask & (IN_Q_OVERFLOW | IN_IGNORED)) {
				errno = EOVERFLOW;
				return fail(
					"inotify overflow or watch disappeared");
			}
			for (int i = 0; i < 3; ++i)
				if (ev->wd == groups[i].wd &&
				    (ev->mask & IN_MODIFY))
					*seen |= 1u << i;
			off += sizeof(*ev) + ev->len;
		}
	}
}

/* Collect required edges and allow asynchronous kernfs notifications to settle.
 * Absence checks cover this bounded quiet interval, not an unlimited future.
 */
static int expect_events(unsigned int required, unsigned int forbidden)
{
	unsigned int seen = 0;
	int64_t start = now_ms();
	if (start < 0)
		return fail("clock_gettime");
	int64_t deadline = start + TIMEOUT_MS;
	int64_t quiet = required ? -1 : start + QUIET_MS;
	for (;;) {
		if (interrupted) {
			errno = EINTR;
			return fail("interrupted");
		}
		if (seen & forbidden) {
			fprintf(stderr, "notified=0x%x forbidden=0x%x\n", seen,
				forbidden);
			errno = EPROTO;
			return fail("unchanged cgroup received IN_MODIFY");
		}
		int64_t now = now_ms();
		if (now < 0)
			return fail("clock_gettime");
		if ((seen & required) == required && quiet < 0)
			quiet = now + QUIET_MS;
		if (quiet >= 0 && now >= quiet) {
			printf("PASS phase=%s IN_MODIFY=0x%x required=0x%x\n",
			       phase, seen, required);
			return 0;
		}
		if (now >= deadline) {
			fprintf(stderr, "notified=0x%x required=0x%x\n", seen,
				required);
			errno = ETIMEDOUT;
			return fail("epoll IN_MODIFY timeout (3000ms)");
		}
		int64_t until = quiet >= 0 && quiet < deadline ? quiet :
								 deadline;
		struct epoll_event event;
		int n = epoll_wait(epoll_fd, &event, 1, (int)(until - now));
		if (n < 0 && errno == EINTR)
			continue;
		if (n < 0)
			return fail("epoll_wait");
		if (n &&
		    (event.data.fd != notify_fd || !(event.events & EPOLLIN))) {
			errno = EPROTO;
			return fail("unexpected epoll event");
		}
		/* Read only after epoll reports readiness: queued inotify bytes alone
		 * must not hide a broken inotify-to-epoll wakeup path.
		 */
		if (n && drain_events(&seen, deadline) < 0)
			return -1;
	}
}

static int spawn_child(int index)
{
	int control[2];
	if (pipe2(control, O_CLOEXEC) < 0)
		return fail("pipe2");
	pid_t pid = fork();
	if (pid < 0) {
		int err = errno;
		close(control[0]);
		close(control[1]);
		errno = err;
		return fail("fork");
	}
	if (pid == 0) {
		close(control[1]);
		close(notify_fd);
		close(epoll_fd);
		for (int i = 0; i < 2; ++i)
			if (children[i].control >= 0)
				close(children[i].control);
		char command;
		ssize_t n;
		do {
			n = read(control[0], &command, 1);
		} while (n < 0 && errno == EINTR);
		close(control[0]);
		_exit(n >= 0 ? 0 : 1);
	}
	close(control[0]);
	children[index].pid = pid;
	children[index].control = control[1];
	return 0;
}

static int move_child(int index, int destination)
{
	char path[PATH_MAX], pid[32];
	if (make_path(path, sizeof(path), groups[destination].dir,
		      "cgroup.procs") < 0)
		return -1;
	int fd = open(path, O_WRONLY | O_CLOEXEC);
	if (fd < 0)
		return fail(path);
	int len =
		snprintf(pid, sizeof(pid), "%ld\n", (long)children[index].pid);
	ssize_t n;
	do {
		n = write(fd, pid, (size_t)len);
	} while (n < 0 && errno == EINTR && !interrupted);
	int err = errno;
	close(fd);
	if (n != len) {
		errno = n < 0 ? err : EIO;
		return fail("write child PID to cgroup.procs");
	}
	return 0;
}

/* Observe a zombie without reaping: populated must track live tasks, not reap. */
static int exit_without_reap(int index)
{
	if (write(children[index].control, "x", 1) != 1)
		return fail("release child pipe");
	close(children[index].control);
	children[index].control = -1;
	int64_t start = now_ms();
	if (start < 0)
		return fail("clock_gettime");
	int64_t deadline = start + TIMEOUT_MS;
	for (;;) {
		siginfo_t info = { 0 };
		if (waitid(P_PID, children[index].pid, &info,
			   WEXITED | WNOWAIT | WNOHANG) < 0)
			return fail("waitid WNOWAIT");
		if (info.si_pid == children[index].pid) {
			if (info.si_code != CLD_EXITED || info.si_status != 0) {
				errno = ECHILD;
				return fail("child did not exit normally");
			}
			return 0;
		}
		int64_t now = now_ms();
		if (now < 0)
			return fail("clock_gettime");
		if (interrupted || now >= deadline) {
			errno = interrupted ? EINTR : ETIMEDOUT;
			return fail("child exit timeout (3000ms)");
		}
		struct timespec pause = { 0, 1000000 };
		nanosleep(&pause, NULL);
	}
}

static int cleanup(void)
{
	int failed = 0;
	phase = "cleanup";
	for (int i = 0; i < 2; ++i) {
		if (children[i].control >= 0)
			close(children[i].control);
		if (children[i].pid <= 0)
			continue;
		kill(children[i].pid, SIGKILL);
		int64_t start = now_ms();
		if (start < 0) {
			fail("clock_gettime cleanup");
			failed = 1;
			continue;
		}
		int64_t deadline = start + TIMEOUT_MS;
		for (;;) {
			pid_t got = waitpid(children[i].pid, NULL, WNOHANG);
			if (got == children[i].pid ||
			    (got < 0 && errno == ECHILD))
				break;
			if (got < 0 && errno != EINTR) {
				fail("waitpid cleanup");
				failed = 1;
				break;
			}
			int64_t now = now_ms();
			if (now < 0) {
				fail("clock_gettime cleanup");
				failed = 1;
				break;
			}
			if (now >= deadline) {
				errno = ETIMEDOUT;
				fail("child cleanup timeout (3000ms)");
				failed = 1;
				break;
			}
			struct timespec pause = { 0, 1000000 };
			nanosleep(&pause, NULL);
		}
	}
	if (epoll_fd >= 0)
		close(epoll_fd);
	if (notify_fd >= 0)
		close(notify_fd);
	for (int i = 2; i >= 0; --i) {
		if (groups[i].created && rmdir(groups[i].dir) < 0) {
			fail(groups[i].dir);
			failed = 1;
		}
	}
	return failed ? -1 : 0;
}

#define TRY(expr)                 \
	do {                      \
		if ((expr) < 0)   \
			goto out; \
	} while (0)

int main(int argc, char **argv)
{
	const char *parent = argc == 2 ? argv[1] : "/sys/fs/cgroup";
	int result = 1;
	char name[96], original[4096], current[4096];
	struct statfs fs;
	setvbuf(stdout, NULL, _IOLBF, 0);
	if (argc > 2) {
		fprintf(stderr, "usage: %s [writable-cgroup-v2-parent]\n",
			argv[0]);
		return 2;
	}
	signal(SIGCHLD, SIG_DFL);
	signal(SIGPIPE, SIG_IGN);
	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);
	if (statfs(parent, &fs) < 0 ||
	    (unsigned long)fs.f_type != CGROUP2_MAGIC) {
		fprintf(stderr,
			"SKIP: parent is not accessible cgroup v2: %s\n",
			parent);
		return 77;
	}
	TRY(read_text("/proc/self/cgroup", original, sizeof(original)));
	snprintf(name, sizeof(name), "k1-events-%ld-XXXXXX", (long)getpid());
	TRY(make_path(groups[0].dir, sizeof(groups[0].dir), parent, name));
	if (!mkdtemp(groups[0].dir)) {
		if (errno == EROFS || errno == EACCES || errno == EPERM) {
			fprintf(stderr,
				"SKIP: cgroup parent not writable: %s errno=%d (%s)\n",
				parent, errno, strerror(errno));
			result = 77;
			goto out;
		}
		fail("mkdtemp cgroup subtree");
		goto out;
	}
	groups[0].created = 1;
	printf("test subtree=%s parent_pid=%ld (parent remains in original group)\n",
	       groups[0].dir, (long)getpid());
	for (int i = 1; i < 3; ++i) {
		TRY(make_path(groups[i].dir, sizeof(groups[i].dir),
			      groups[0].dir, i == 1 ? "a" : "b"));
		if (mkdir(groups[i].dir, 0755) < 0) {
			fail("mkdir child cgroup");
			goto out;
		}
		groups[i].created = 1;
	}
	notify_fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
	if (notify_fd < 0) {
		fail("inotify_init1");
		goto out;
	}
	epoll_fd = epoll_create1(EPOLL_CLOEXEC);
	if (epoll_fd < 0) {
		fail("epoll_create1");
		goto out;
	}
	struct epoll_event event = { .events = EPOLLIN, .data.fd = notify_fd };
	if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, notify_fd, &event) < 0) {
		fail("epoll_ctl");
		goto out;
	}
	for (int i = 0; i < 3; ++i) {
		TRY(make_path(groups[i].events, sizeof(groups[i].events),
			      groups[i].dir, "cgroup.events"));
		groups[i].wd = inotify_add_watch(notify_fd, groups[i].events,
						 IN_MODIFY);
		if (groups[i].wd < 0) {
			fail("inotify_add_watch");
			goto out;
		}
		for (int j = 0; j < i; ++j) {
			if (groups[i].wd == groups[j].wd) {
				errno = EPROTO;
				fail("distinct cgroups share watch identity");
				goto out;
			}
		}
	}
	TRY(expect_state(0, 0, 0));
	TRY(check_identity());
	TRY(expect_events(0, ROOT_BIT | A_BIT | B_BIT));
	TRY(spawn_child(0));
	TRY(spawn_child(1));

	begin_phase("first child enters A: parent/A 0-to-1");
	TRY(move_child(0, 1));
	TRY(expect_state(1, 1, 0));
	TRY(expect_events(ROOT_BIT | A_BIT, B_BIT));
	TRY(check_identity());

	begin_phase("second child enters A: populated remains 1");
	TRY(move_child(1, 1));
	TRY(expect_state(1, 1, 0));
	/* A 1-to-1 move has no populated edge on any watched group. */
	TRY(expect_events(0, ROOT_BIT | A_BIT | B_BIT));
	TRY(check_identity());

	begin_phase("first sibling move A-to-B: B 0-to-1, ancestor stays 1");
	TRY(move_child(0, 2));
	TRY(expect_state(1, 1, 1));
	TRY(expect_events(B_BIT, ROOT_BIT | A_BIT));
	TRY(check_identity());

	begin_phase("last sibling move A-to-B: A 1-to-0, ancestor stays 1");
	TRY(move_child(1, 2));
	TRY(expect_state(1, 0, 1));
	TRY(expect_events(A_BIT, ROOT_BIT | B_BIT));
	TRY(check_identity());

	begin_phase("first unreaped exit: B remains populated");
	TRY(exit_without_reap(0));
	TRY(expect_state(1, 0, 1));
	TRY(expect_events(0, ROOT_BIT | A_BIT | B_BIT));

	begin_phase("last unreaped exit: parent/B 1-to-0 before reap");
	TRY(exit_without_reap(1));
	/* Zombie visibility may precede the final cgroup detach. Wait for its
	 * notification before reading 0, without ever reaping either child.
	 */
	TRY(expect_events(ROOT_BIT | B_BIT, A_BIT));
	TRY(expect_state(0, 0, 0));
	TRY(check_identity());
	TRY(read_text("/proc/self/cgroup", current, sizeof(current)));
	if (strcmp(original, current)) {
		errno = EPROTO;
		fail("parent cgroup changed");
		goto out;
	}
	result = 0;
out:
	if (cleanup() < 0)
		result = 1;
	if (result == 0)
		puts("PASS: cgroup.events notification, identity, sibling migration and unreaped exit");
	return result;
}