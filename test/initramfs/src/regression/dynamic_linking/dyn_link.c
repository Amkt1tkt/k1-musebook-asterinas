// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: auxv / ld.so completeness check for dynamically linked userland.

// Checks that this program really went through the ELF interpreter and that the
// interpreter got everything it needs from the kernel.
//
// A plain dynamically linked "hello world" already proves that `ld.so` ran, but
// it does not say *which* auxiliary vector entries arrived, and a missing entry
// can stay invisible until some library (Mesa, in our case) happens to read it.
// So this test inspects the auxv directly and fails loudly on the entries that
// `ld.so` and libc actually depend on.

// `dl_iterate_phdr` and `struct dl_phdr_info` are GNU extensions.
#define _GNU_SOURCE

#include <dlfcn.h>
#include <elf.h>
#include <link.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>

static int failures;

static void check(int ok, const char *what, const char *detail)
{
	if (ok) {
		printf("ok   %s (%s)\n", what, detail);
	} else {
		printf("FAIL %s (%s)\n", what, detail);
		failures++;
	}
}

// The interpreter must have been mapped somewhere and told us where.
static void check_interpreter_ran(void)
{
	char buf[64];
	unsigned long base = getauxval(AT_BASE);

	// AT_BASE is the interpreter load address. A statically linked program
	// gets 0 here; anything else means an interpreter was mapped.
	snprintf(buf, sizeof(buf), "AT_BASE = %#lx", base);
	check(base != 0, "interpreter was loaded", buf);
}

// Walk our own program headers via the addresses the kernel handed us. If AT_PHDR
// or AT_PHNUM were wrong, `ld.so` could not have found PT_DYNAMIC, so this both
// validates the values and demonstrates the layout `ld.so` relies on.
static void check_program_headers(void)
{
	unsigned long phdr = getauxval(AT_PHDR);
	unsigned long phent = getauxval(AT_PHENT);
	unsigned long phnum = getauxval(AT_PHNUM);
	char buf[128];

	snprintf(buf, sizeof(buf), "AT_PHDR = %#lx", phdr);
	check(phdr != 0, "AT_PHDR present", buf);

	snprintf(buf, sizeof(buf), "AT_PHENT = %lu, expected %zu", phent,
		 sizeof(ElfW(Phdr)));
	check(phent == sizeof(ElfW(Phdr)), "AT_PHENT matches ElfW(Phdr)", buf);

	snprintf(buf, sizeof(buf), "AT_PHNUM = %lu", phnum);
	check(phnum != 0, "AT_PHNUM present", buf);

	if (phdr == 0 || phnum == 0 || phent != sizeof(ElfW(Phdr)))
		return;

	// PT_INTERP must be among them: that is the header that made the kernel
	// load an interpreter in the first place.
	const ElfW(Phdr) *headers = (const ElfW(Phdr) *)phdr;
	int saw_interp = 0;
	int saw_dynamic = 0;
	for (unsigned long i = 0; i < phnum; i++) {
		if (headers[i].p_type == PT_INTERP)
			saw_interp = 1;
		if (headers[i].p_type == PT_DYNAMIC)
			saw_dynamic = 1;
	}
	check(saw_interp, "PT_INTERP found via AT_PHDR",
	      "walked our own phdrs");
	check(saw_dynamic, "PT_DYNAMIC found via AT_PHDR",
	      "walked our own phdrs");
}

// AT_RANDOM backs the stack protector canary and malloc's randomisation. If it is
// absent, glibc dereferences a null pointer during early startup.
static void check_at_random(void)
{
	unsigned long random = getauxval(AT_RANDOM);
	char buf[64];

	snprintf(buf, sizeof(buf), "AT_RANDOM = %#lx", random);
	check(random != 0, "AT_RANDOM present", buf);

	if (random == 0)
		return;

	// It must be readable: glibc copies 16 bytes out of it. Fold them into a
	// checksum so the copy cannot be optimised away.
	unsigned char bytes[16];
	unsigned int sum = 0;
	char buf2[64];

	memcpy(bytes, (const void *)random, sizeof(bytes));
	for (size_t i = 0; i < sizeof(bytes); i++)
		sum += bytes[i];

	snprintf(buf2, sizeof(buf2), "read 16 bytes, checksum %u", sum);
	check(1, "AT_RANDOM readable", buf2);
}

static void check_misc_auxv(void)
{
	char buf[96];
	unsigned long pagesz = getauxval(AT_PAGESZ);
	unsigned long entry = getauxval(AT_ENTRY);

	snprintf(buf, sizeof(buf), "AT_PAGESZ = %lu", pagesz);
	check(pagesz == 4096, "AT_PAGESZ is 4096", buf);

	snprintf(buf, sizeof(buf), "AT_ENTRY = %#lx", entry);
	check(entry != 0, "AT_ENTRY present", buf);

	// Not fatal if absent (glibc falls back to real syscalls), but Mesa's
	// clock-heavy paths care about it, so report what we got.
	unsigned long vdso = getauxval(AT_SYSINFO_EHDR);
	snprintf(buf, sizeof(buf), "AT_SYSINFO_EHDR = %#lx%s", vdso,
		 vdso ? "" : " (absent; clock calls will be real syscalls)");
	printf("info %s\n", buf);
}

// Symbol resolution through the interpreter, both the eager kind (this call was
// resolved via the PLT) and the lazy kind (`dlopen`/`dlsym` at runtime).
static void check_symbol_resolution(void)
{
	// `dlopen(NULL, ...)` reuses the already-loaded main program's scope, so
	// this exercises the interpreter's symbol lookup without needing any
	// extra library to be present in the initramfs.
	void *handle = dlopen(NULL, RTLD_NOW);
	check(handle != NULL, "dlopen(NULL) succeeded",
	      handle ? "global scope opened" : dlerror());
	if (handle == NULL)
		return;

	void *sym = dlsym(handle, "printf");
	check(sym != NULL, "dlsym found a libc symbol",
	      sym ? "resolved printf" : dlerror());

	dlclose(handle);
}

// Confirm the link map is populated, i.e. the interpreter recorded the objects it
// loaded. This is the structure Mesa's loader walks when it probes for drivers.
static int count_objects(struct dl_phdr_info *info, size_t size, void *data)
{
	(void)size;
	int *count = data;
	(*count)++;
	if (info->dlpi_name != NULL && info->dlpi_name[0] != '\0')
		printf("info   loaded object: %s\n", info->dlpi_name);
	return 0;
}

static void check_link_map(void)
{
	int count = 0;
	char buf[64];

	dl_iterate_phdr(count_objects, &count);
	snprintf(buf, sizeof(buf), "%d objects in the link map", count);
	// The main program plus at least libc and the interpreter itself.
	check(count >= 3, "link map is populated", buf);
}

int main(void)
{
	printf("dynamic linking checks\n");

	check_interpreter_ran();
	check_program_headers();
	check_at_random();
	check_misc_auxv();
	check_symbol_resolution();
	check_link_map();

	if (failures != 0) {
		printf("dyn_link: %d check(s) failed\n", failures);
		return 1;
	}
	printf("dyn_link: all checks passed\n");
	return 0;
}
