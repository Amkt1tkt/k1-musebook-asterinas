// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: regression for the futex lost-wakeup (WAIT enqueue under lock; WAKE dequeues).
//
// Regression for the kernel futex lost-wakeup: WAIT must compare *uaddr and
// enqueue under the same bucket lock, and WAKE must dequeue matching waiters.
// Waiters must observe counter progress rather than sleep past a matching wake.

#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#ifndef SYS_futex
#error "SYS_futex unavailable on this system"
#endif

#define FUTEX_WAIT 0
#define FUTEX_WAKE 1
#define FUTEX_PRIVATE_FLAG 128

_Static_assert(sizeof(atomic_uint) == sizeof(uint32_t),
	       "futex word must be 32 bits");

#define NWAITERS 4
#define TARGET 8
#define WATCHDOG_SECS 8

static atomic_uint word;
static atomic_int finished;

static int sys_futex(atomic_uint *uaddr, int op, uint32_t val)
{
	return (int)syscall(SYS_futex, uaddr, op, val, NULL, NULL, 0);
}

static int futex_wait(atomic_uint *uaddr, uint32_t expected)
{
	return sys_futex(uaddr, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, expected);
}

static int futex_wake(atomic_uint *uaddr, int n)
{
	return sys_futex(uaddr, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, (uint32_t)n);
}

static void *waiter_main(void *arg)
{
	(void)arg;
	for (;;) {
		uint32_t val =
			atomic_load_explicit(&word, memory_order_relaxed);
		if (val >= TARGET) {
			atomic_fetch_add(&finished, 1);
			return NULL;
		}
		int rc = futex_wait(&word, val);
		if (rc < 0 && errno != EAGAIN && errno != EINTR &&
		    errno != ETIMEDOUT) {
			fprintf(stderr, "futex_wait: %s\n", strerror(errno));
			exit(1);
		}
	}
}

static void *watchdog_main(void *arg)
{
	(void)arg;
	sleep(WATCHDOG_SECS);
	if (atomic_load(&finished) < NWAITERS) {
		fprintf(stderr,
			"ERROR: lost-wake: %d/%d waiters still parked, word=%u\n",
			NWAITERS - atomic_load(&finished), NWAITERS,
			atomic_load(&word));
		exit(1);
	}
	return NULL;
}

int main(void)
{
	pthread_t watchdog;
	pthread_t threads[NWAITERS];
	int error = pthread_create(&watchdog, NULL, watchdog_main, NULL);
	if (error != 0) {
		fprintf(stderr, "pthread_create watchdog: %s\n",
			strerror(error));
		return 1;
	}
	error = pthread_detach(watchdog);
	if (error != 0) {
		fprintf(stderr, "pthread_detach watchdog: %s\n",
			strerror(error));
		return 1;
	}

	/* The watchdog also bounds the initial mismatched WAIT regression. */
	uint32_t before = 1;
	atomic_store(&word, before);
	errno = 0;
	if (futex_wait(&word, 0) != -1 || errno != EAGAIN) {
		fprintf(stderr,
			"ERROR: WAIT on a changed word must be EAGAIN\n");
		return 1;
	}

	atomic_store(&word, 0);
	atomic_store(&finished, 0);

	for (int i = 0; i < NWAITERS; i++) {
		error = pthread_create(&threads[i], NULL, waiter_main, NULL);
		if (error != 0) {
			fprintf(stderr, "pthread_create waiter: %s\n",
				strerror(error));
			return 1;
		}
	}

	/* Give the waiters a chance to park, then bump the counter like
	 * a single-wake producer. The final value satisfies every waiter, so
	 * broadcast at that point; WAKE(1) alone may legally leave others asleep. */
	usleep(50 * 1000);
	for (unsigned i = 0; i < TARGET; i++) {
		atomic_fetch_add_explicit(&word, 1, memory_order_relaxed);
		if (futex_wake(&word, i + 1 == TARGET ? NWAITERS : 1) < 0) {
			perror("futex_wake");
			return 1;
		}
	}

	for (int i = 0; i < NWAITERS; i++) {
		error = pthread_join(threads[i], NULL);
		if (error != 0) {
			fprintf(stderr, "pthread_join: %s\n", strerror(error));
			return 1;
		}
	}

	if (atomic_load(&finished) != NWAITERS) {
		fprintf(stderr, "ERROR: not all waiters finished\n");
		return 1;
	}

	printf("All tests passed.\n");
	return 0;
}
