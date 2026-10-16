# SPDX-License-Identifier: MPL-2.0

# K1 MUSE Book: dynamically linked ELF + FPU thread tests (libdrm probe is opt-in).

# No `-static` here on purpose: these tests are only meaningful when the ELF
# interpreter maps them, so they must stay dynamically linked.
#
# `-O2` is required rather than cosmetic: `fpu_threads` relies on the compiler
# keeping its accumulators in callee-saved FP registers across `sched_yield`,
# which is the state a context switch has to preserve. At `-O0` everything is
# spilled to the stack on every statement and the test would pass even on a
# kernel that saves no FP state at all.
#
# `-lm` is for `fpu_threads`: `fesetround` and `fegetround` come from `fenv.h`,
# and POSIX puts the whole of that header in libm.
EXTRA_C_FLAGS := -O2 -lpthread -ldl -lm

include ../common/Makefile
