// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: probe libdrm loading and calls independently of device enumeration.

// Checks libdrm soname loading, symbol resolution and a library call.
// Device enumeration may fail or find no devices without failing this probe.
// See README.md for its deliberately limited scope and opt-in build.

#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>

#include <xf86drm.h>

#define MAX_DEVICES 16

int main(void)
{
	printf("libdrm dynamic linking probe\n");

	// 1. Resolution at load time. We are linked against libdrm, so if the
	//    interpreter could not satisfy that dependency we would never have
	//    reached main() at all. Reaching here is itself the first result.
	printf("ok   libdrm.so.2 was loaded by the interpreter (we reached main)\n");

	// 2. Resolution by soname at run time, which is the path Mesa's loader
	//    takes when it dlopen()s a driver.
	void *handle = dlopen("libdrm.so.2", RTLD_NOW);
	if (handle == NULL) {
		printf("FAIL dlopen(\"libdrm.so.2\") failed: %s\n", dlerror());
		printf("drm_probe: dynamic linking problem\n");
		return 1;
	}
	printf("ok   dlopen(\"libdrm.so.2\") succeeded\n");

	void *sym = dlsym(handle, "drmGetDevices2");
	if (sym == NULL) {
		printf("FAIL dlsym(\"drmGetDevices2\") failed: %s\n",
		       dlerror());
		printf("drm_probe: symbol resolution problem\n");
		dlclose(handle);
		return 1;
	}
	printf("ok   dlsym(\"drmGetDevices2\") resolved\n");
	dlclose(handle);

	// 3. Actually call into the library. Any return value proves the call
	//    and return succeeded across the dynamically linked interface.
	drmDevicePtr devices[MAX_DEVICES];
	memset(devices, 0, sizeof(devices));

	int ret = drmGetDevices2(0, devices, MAX_DEVICES);

	printf("info drmGetDevices2 returned %d\n", ret);
	if (ret < 0) {
		printf("info   (%s) - device enumeration failed; linking still worked\n",
		       strerror(-ret));
	} else if (ret == 0) {
		printf("info   no DRM devices; this is a linking-only test\n");
	} else {
		printf("info   found %d DRM device(s)\n", ret);
		drmFreeDevices(devices, ret);
	}

	// Reaching this point means the whole dynamic linking chain worked.
	printf("ok   called into libdrm and returned\n");
	printf("drm_probe: dynamic linking against libdrm works\n");
	return 0;
}
