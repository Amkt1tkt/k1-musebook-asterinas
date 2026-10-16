<!-- SPDX-License-Identifier: MPL-2.0 -->
<!-- K1 MUSE Book: scope and execution of the libdrm loading probe. -->

# libdrm dynamic-linking probe

Checks loader resolution of libdrm.so.2, dlopen/dlsym, and a real call to
drmGetDevices2. A negative or zero enumeration result is still a linking PASS;
it does not prove sysfs, GPU rendering, or device enumeration works.

This subdirectory remains opt-in: the generic dynamic_linking Makefile and
run_test.sh do not include it, and the default regression Nix recipe does not
provide libdrm. That separation keeps generic CI independent of graphics
packages.

To build manually in a development environment with libdrm headers/libraries:

```sh
cc -Wall -Wextra -Werror $(pkg-config --cflags libdrm) drm_probe.c \
   -o drm_probe $(pkg-config --libs libdrm) -ldl
./drm_probe
```

For automated initramfs inclusion, provide target libdrm build/runtime inputs
through the regression Nix recipe and add this subdirectory and executable to
the parent Makefile/run_test.sh together. Do not assume a host-built binary is
a RISC-V test artifact.

Load/resolve failure returns nonzero. Enumeration failure is printed as
information; investigate device access and sysfs separately. This test does
not prove every Mesa dependency loads.
