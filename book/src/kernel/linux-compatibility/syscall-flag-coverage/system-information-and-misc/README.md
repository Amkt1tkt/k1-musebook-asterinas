# System Information & Misc.

<!--
Put system calls such as
uname, getrlimit, reboot, setrlimit, sysinfo, times, gettimeofday, clock_gettime,
clock_settime, getrusage, getdents, getdents64, personality, syslog,
arch_prctl, set_tid_address, and getrandom
under this category.
-->

### `arch_prctl`

Supported functionality in SCML:

```c
{{#include arch_prctl.scml}}
```

Unsupported codes:
* `ARCH_GET_CPUID` and `ARCH_SET_CPUID`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/arch_prctl.2.html).

### `getrusage`

Supported functionality in SCML:

```c
{{#include getrusage.scml}}
```

Unsupported `who` flags:
* `RUSAGE_CHILDREN`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/getrusage.2.html).

### `getrandom`

Supported functionality in SCML:

```c
{{#include getrandom.scml}}
```

Silently-ignored flags:
* `GRND_NONBLOCK` because the underlying operation never blocks

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/getrandom.2.html).

### `reboot`

Supported functionality in SCML:

```c
{{#include reboot.scml}}
```

Unsupported `op` flags:
* `LINUX_REBOOT_CMD_CAD_OFF`
* `LINUX_REBOOT_CMD_CAD_ON`
* `LINUX_REBOOT_CMD_KEXEC`
* `LINUX_REBOOT_CMD_RESTART2`
* `LINUX_REBOOT_CMD_SW_SUSPEND`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/reboot.2.html).

### `sysinfo`

Supported functionality in SCML:

```c
{{#include sysinfo.scml}}
```

Supported returned fields:
* `uptime`, rounded up to seconds when it has a fractional part
* `loads`, containing the 1-, 5-, and 15-minute load averages as fixed-point values scaled by `1 << SI_LOAD_SHIFT`
* `totalram`, `freeram`, `procs`, and `mem_unit`

The `sharedram`, `bufferram`, `totalhigh`, and `freehigh` fields are reported as zero.
The `totalswap` and `freeswap` fields are reported as zero because swap is not supported.

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/sysinfo.2.html).

## POSIX clocks

### `clock_gettime`

Supported functionality in SCML:

```c
{{#include clock_gettime.scml}}
```

Unsupported predefined clock IDs:
* `CLOCK_REALTIME_ALARM`
* `CLOCK_BOOTTIME_ALARM`
* `CLOCK_TAI`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/clock_gettime.2.html).

<!-- K1 MUSE Book --- (document the realtime setter and its authority/range contract) -->
### `clock_settime`

Supported functionality in SCML:

```c
{{#include clock_settime.scml}}
```

Only `CLOCK_REALTIME` can be set.
The caller needs `CAP_SYS_TIME` in the initial user namespace.
Seconds must be nonnegative and nanoseconds must lie in `[0, 1000000000)`.
Values outside the kernel's bounded realtime range,
or earlier than the current `CLOCK_MONOTONIC`, are rejected.
Successful calls update realtime and its coarse/vDSO representations;
monotonic time and the boot-time sample remain unchanged.
The call does not write the RTC; persistence uses `RTC_SET_TIME` or the userspace hwclock service.
<!-- K1 MUSE Book --- -->

### `clock_nanosleep`

Supported functionality in SCML:

```c
{{#include clock_nanosleep.scml}}
```

Unsupported clock IDs:
* `CLOCK_TAI`

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/clock_nanosleep.2.html).

<!-- K1 MUSE Book --- (document legacy UTC RTC initialization without a local-time warp) -->
### `settimeofday`

Supported functionality in SCML:

```c
{{#include settimeofday.scml}}
```

The caller needs `CAP_SYS_TIME` in the initial user namespace,
including for a timezone-only call.
Either pointer may be `NULL`; a supplied `tv` needs nonnegative seconds
and microseconds in `[0, 1000000)`.
The realtime range and RTC persistence limits are the same as for `clock_settime`.

A supplied `tz` updates shared timezone metadata;
`tz_minuteswest` must lie in `[-900, 900]`.
The boot RTC is interpreted as UTC, and Linux's first-call local-RTC clock warp is unsupported.
A first timezone-only call with a nonzero offset returns `EOPNOTSUPP`.
A zero-offset initialization or a call supplying both `tv` and `tz` avoids that warp;
subsequent timezone metadata updates do not move realtime.
<!-- K1 MUSE Book --- -->
