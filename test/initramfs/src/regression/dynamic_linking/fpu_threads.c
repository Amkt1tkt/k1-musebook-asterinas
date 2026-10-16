// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: FPU register / fcsr isolation across context switches.

// Catches a kernel that lets user-mode floating-point state leak between threads.
//
// The dangerous failure mode is not a crash. If the kernel enables the FPU for
// user mode but forgets to save and restore `f0`-`f31` and `fcsr` across a
// context switch, every thread silently reads whatever the previously scheduled
// thread left in the register file. Arithmetic keeps "working", the answers are
// just wrong, and nothing reports an error. On riscv64 this matters because the
// kernel itself is built soft-float while user space (glibc `lp64d`, and later
// Mesa) is hard-float.
//
// Two independent checks:
//
//  1. `fcsr` preservation. Each thread selects a different rounding mode and
//     re-reads it after many scheduling points.
//  2. Data register preservation. Each thread advances twelve independent
//     accumulators in a loop that calls `sched_yield` on every iteration. Twelve
//     is chosen to match the twelve callee-saved FP registers on riscv64
//     (`fs0`-`fs11`), to encourage register allocation across the yield. The
//     compiler may spill some values; this C test does not prove coverage of
//     every FP register. Results are compared against the same computation run
//     single-threaded beforehand.
//
// The arithmetic is a real dependent chain rather than a constant, so the
// compiler cannot fold the check away, and `sched_yield` is opaque to it, so the
// values cannot be sunk to after the loop.

#include <fenv.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define NR_THREADS 4
// Encourage pressure on the twelve callee-saved FP registers on riscv64.
#define NR_ACCUMULATORS 12
#define NR_ROUNDS 4000

// Bounded and deterministic: the value is multiplied up and brought back down,
// so it stays near its seed while still exercising fmul/fadd/fsub/fdiv and
// accumulating a reproducible rounding drift.
static inline double step(double a)
{
	a = a * 1.0000001 + 0.5;
	return (a - 0.5) / 1.0000001;
}

struct chain_result {
	double v[NR_ACCUMULATORS];
};

// `do_yield` is off for the single-threaded reference run and on for the
// concurrent run; yielding must not change the arithmetic.
static void run_chain(int id, int do_yield, struct chain_result *out)
{
	double a0 = 1.0 + (double)id;
	double a1 = a0 + 1.0;
	double a2 = a0 + 2.0;
	double a3 = a0 + 3.0;
	double a4 = a0 + 4.0;
	double a5 = a0 + 5.0;
	double a6 = a0 + 6.0;
	double a7 = a0 + 7.0;
	double a8 = a0 + 8.0;
	double a9 = a0 + 9.0;
	double a10 = a0 + 10.0;
	double a11 = a0 + 11.0;

	for (int round = 0; round < NR_ROUNDS; round++) {
		a0 = step(a0);
		a1 = step(a1);
		a2 = step(a2);
		a3 = step(a3);
		a4 = step(a4);
		a5 = step(a5);
		a6 = step(a6);
		a7 = step(a7);
		a8 = step(a8);
		a9 = step(a9);
		a10 = step(a10);
		a11 = step(a11);

		if (do_yield) {
			// Invite a context switch while all twelve accumulators
			// are live (register allocation is compiler-dependent).
			sched_yield();
		}
	}

	out->v[0] = a0;
	out->v[1] = a1;
	out->v[2] = a2;
	out->v[3] = a3;
	out->v[4] = a4;
	out->v[5] = a5;
	out->v[6] = a6;
	out->v[7] = a7;
	out->v[8] = a8;
	out->v[9] = a9;
	out->v[10] = a10;
	out->v[11] = a11;
}

struct chain_arg {
	int id;
	struct chain_result got;
	int round_error;
};

static void *chain_worker(void *arg)
{
	struct chain_arg *c = arg;

	// Pin the rounding mode so this check stays independent of whatever the
	// rounding-mode check did earlier.
	if (fesetround(FE_TONEAREST) != 0) {
		c->round_error = 1;
		return NULL;
	}
	run_chain(c->id, 1, &c->got);
	return NULL;
}

struct round_arg {
	int id;
	int want;
	int mismatch;
	int observed;
};

static void *round_worker(void *arg)
{
	struct round_arg *r = arg;

	if (fesetround(r->want) != 0) {
		r->mismatch = 1;
		r->observed = -1;
		return NULL;
	}

	for (int i = 0; i < NR_ROUNDS; i++) {
		sched_yield();

		int now = fegetround();
		if (now != r->want && !r->mismatch) {
			r->mismatch = 1;
			r->observed = now;
		}
	}

	return NULL;
}

/* Never return past stack arguments while a created worker can still use them. */
static void join_created(pthread_t *threads, int count)
{
	for (int i = 0; i < count; ++i) {
		int error = pthread_join(threads[i], NULL);
		if (error != 0) {
			fprintf(stderr, "FAIL pthread_join: %s\n",
				strerror(error));
			exit(EXIT_FAILURE);
		}
	}
}

static int check_rounding_modes(void)
{
	// Two distinct modes are enough to tell "preserved" from "shared".
	const int modes[NR_THREADS] = { FE_TONEAREST, FE_TOWARDZERO, FE_UPWARD,
					FE_DOWNWARD };
	pthread_t threads[NR_THREADS];
	struct round_arg args[NR_THREADS] = { 0 };
	int failures = 0;

	for (int i = 0; i < NR_THREADS; i++) {
		args[i].id = i;
		args[i].want = modes[i];
		if (pthread_create(&threads[i], NULL, round_worker, &args[i]) !=
		    0) {
			printf("FAIL could not create rounding-mode thread %d\n",
			       i);
			join_created(threads, i);
			return 1;
		}
	}
	join_created(threads, NR_THREADS);

	for (int i = 0; i < NR_THREADS; i++) {
		if (args[i].mismatch) {
			printf("FAIL thread %d lost its rounding mode: wanted %d, saw %d (fcsr not preserved)\n",
			       i, args[i].want, args[i].observed);
			failures++;
		}
	}

	if (failures == 0)
		printf("ok   fcsr (rounding mode) preserved across context switches\n");

	return failures;
}

static int check_data_registers(void)
{
	pthread_t threads[NR_THREADS];
	struct chain_arg args[NR_THREADS] = { 0 };
	struct chain_result expected[NR_THREADS] = { 0 };
	int failures = 0;

	// Reference results, computed with no other thread running.
	if (fesetround(FE_TONEAREST) != 0) {
		fprintf(stderr, "FAIL cannot set reference rounding mode\n");
		return 1;
	}
	for (int i = 0; i < NR_THREADS; i++)
		run_chain(i, 0, &expected[i]);

	for (int i = 0; i < NR_THREADS; i++) {
		args[i].id = i;
		if (pthread_create(&threads[i], NULL, chain_worker, &args[i]) !=
		    0) {
			printf("FAIL could not create chain thread %d\n", i);
			join_created(threads, i);
			return 1;
		}
	}
	join_created(threads, NR_THREADS);

	for (int i = 0; i < NR_THREADS; i++) {
		if (args[i].round_error) {
			fprintf(stderr,
				"FAIL cannot set thread %d rounding mode\n", i);
			failures++;
			continue;
		}
		for (int j = 0; j < NR_ACCUMULATORS; j++) {
			if (args[i].got.v[j] != expected[i].v[j]) {
				printf("FAIL thread %d accumulator %d diverged: expected %.17g, got %.17g\n",
				       i, j, expected[i].v[j],
				       args[i].got.v[j]);
				failures++;
			}
		}
	}

	if (failures == 0)
		printf("ok   FP accumulator values preserved across context switches (%d threads x %d accumulators)\n",
		       NR_THREADS, NR_ACCUMULATORS);

	return failures;
}

int main(void)
{
	int failures = 0;

	printf("floating-point context-switch checks\n");

	failures += check_rounding_modes();
	failures += check_data_registers();

	if (failures != 0) {
		printf("fpu_threads: %d check(s) failed\n", failures);
		return 1;
	}

	printf("fpu_threads: all checks passed\n");
	return 0;
}
