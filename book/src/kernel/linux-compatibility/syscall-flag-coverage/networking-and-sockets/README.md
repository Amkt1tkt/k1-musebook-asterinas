# Networking & Sockets

<!--
Put system calls such as

socket, socketpair, bind, listen, accept, connect, getsockname, getpeername,
sendto, recvfrom, sendmsg, recvmsg, shutdown, setsockopt, getsockopt,
sendmmsg, recvmmsg, accept4, recvmsg, and socketcall
under this category.
-->

## Socket creation

### `socket`

Supported functionality in SCML:

```c
{{#include socket.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/socket.2.html).

### `socketpair`

Supported functionality in SCML:

```c
{{#include socketpair.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/socketpair.2.html).

## Socket setup

### `bind`

Supported functionality in SCML:

```c
{{#include bind.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/bind.2.html).

### `connect`

Supported functionality in SCML:

```c
{{#include connect.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/connect.2.html).

### `accept` and `accept4`

Supported functionality in SCML:

```c
{{#include accept_and_accept4.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/accept.2.html).

## Socket communication

### `sendto`, `sendmsg` and `sendmmsg`

Supported functionality in SCML:

```c
{{#include sendto_and_sendmsg.scml}}
```

<!-- K1 MUSE Book --- (per-call nonblocking I/O and SIGPIPE suppression are supported) -->
<!-- Original upstream unsupported-flag entries:
* `MSG_DONTWAIT`
* `MSG_NOSIGNAL`
-->
Unsupported flags:
* `MSG_CONFIRM`
* `MSG_DONTROUTE`
* `MSG_EOR`
* `MSG_MORE`
* `MSG_CONFIRM`
* `MSG_OOB`
* `MSG_FASTOPEN`

`MSG_DONTWAIT` selects nonblocking behavior for this call without changing the descriptor.
`MSG_NOSIGNAL` is accepted; the current send paths do not generate `SIGPIPE` on `EPIPE`.
UNIX sockets carry `SOL_SOCKET` ancillary messages of type `SCM_RIGHTS` and `SCM_CREDENTIALS`.
Explicit credentials must match the sender's real credentials;
passing a UNIX stream socket itself requires `CAP_SYS_ADMIN` in the initial user namespace.
The SCML header patterns describe these ancillary types, with the UNIX-socket restriction stated here.
<!-- K1 MUSE Book --- -->

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/sendto.2.html).

### `recvfrom` and `recvmsg`

Supported functionality in SCML:

```c
{{#include recvfrom_and_recvmsg.scml}}
```

<!-- K1 MUSE Book --- (document receive-call flags and received-descriptor ownership) -->
`MSG_DONTWAIT` selects nonblocking behavior for this call.
`MSG_CMSG_CLOEXEC` applies `FD_CLOEXEC` to the descriptors installed by received UNIX `SCM_RIGHTS` messages.
`SCM_CREDENTIALS` delivery depends on the socket's `SO_PASSCRED` setting.
Ancillary delivery is limited to these `SOL_SOCKET` UNIX message types;
an insufficient control buffer is reported with `MSG_CTRUNC`.
<!-- K1 MUSE Book --- -->

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/recvfrom.2.html).

## Socket options

### `getsockopt` and `setsockopt`

Supported functionality in SCML:

```c
{{#include getsockopt_and_setsockopt.scml}}
```

For more information,
see [the man page](https://man7.org/linux/man-pages/man2/getsockopt.2.html).
