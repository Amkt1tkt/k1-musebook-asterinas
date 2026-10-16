// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: validate the port's reduced netlink filter socket-option ABI.
// This checks validation/acceptance, not execution of BPF instructions.

#include <linux/bpf_common.h>
#include <linux/filter.h>
#include <linux/netlink.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/socket.h>
#include <unistd.h>

#include "../common/test.h"

_Static_assert(sizeof(struct sock_filter) == 8,
	       "struct sock_filter is 8 bytes");
_Static_assert(sizeof(struct sock_fprog) == 16,
	       "LP64 struct sock_fprog is 16 bytes");
_Static_assert(offsetof(struct sock_fprog, filter) == 8,
	       "sock_fprog.filter is at offset 8");
_Static_assert(SO_ATTACH_FILTER == 26, "SO_ATTACH_FILTER");
_Static_assert(SO_DETACH_FILTER == 27, "SO_DETACH_FILTER");
_Static_assert(NETLINK_KOBJECT_UEVENT == 15, "NETLINK_KOBJECT_UEVENT");
_Static_assert(BPF_MAXINSNS == 4096, "BPF_MAXINSNS");

FN_TEST(libudev_attach_filter)
{
	int one = 1;
	struct sockaddr_nl addr = { .nl_family = AF_NETLINK, .nl_groups = 2 };
	socklen_t addrlen = sizeof(addr);
	/* BPF_RET | BPF_K, accept all. */
	struct sock_filter accept_all[] = {
		BPF_STMT(BPF_RET | BPF_K, 0xFFFFFFFFU),
	};
	struct sock_fprog prog = {
		.len = 1,
		.filter = accept_all,
	};
	int dummy = 0;

	int sk = TEST_SUCC(socket(PF_NETLINK, SOCK_DGRAM | SOCK_NONBLOCK,
				  NETLINK_KOBJECT_UEVENT));

	TEST_SUCC(setsockopt(sk, SOL_SOCKET, SO_PASSCRED, &one, sizeof(one)));
	TEST_SUCC(bind(sk, (struct sockaddr *)&addr, sizeof(addr)));
	TEST_RES(getsockname(sk, (struct sockaddr *)&addr, &addrlen),
		 addrlen == sizeof(addr) && addr.nl_groups == 2);

	/* Linux reads an int before checking the exact sock_fprog size. */
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER, &prog,
			      sizeof(prog) - 1),
		   EINVAL);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER, &prog,
			      sizeof(prog) + 1),
		   EINVAL);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER,
			      (void *)(uintptr_t)1, sizeof(prog) - 1),
		   EFAULT);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER,
			      (void *)(uintptr_t)1, sizeof(prog) + 1),
		   EFAULT);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER,
			      (void *)(uintptr_t)1, 0),
		   EINVAL);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_DETACH_FILTER, NULL, 0),
		   EINVAL);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_DETACH_FILTER,
			      (void *)(uintptr_t)1, sizeof(dummy)),
		   EFAULT);
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_DETACH_FILTER, &dummy,
			      sizeof(dummy)),
		   ENOENT);

	struct sock_fprog empty = {
		.len = 0,
		.filter = accept_all,
	};
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER, &empty,
			      sizeof(empty)),
		   EINVAL);

	struct sock_fprog huge = {
		.len = BPF_MAXINSNS + 1,
		.filter = accept_all,
	};
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER, &huge,
			      sizeof(huge)),
		   EINVAL);

	struct sock_fprog bad_ptr = {
		.len = 1,
		.filter = (struct sock_filter *)(uintptr_t)1,
	};
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER, &bad_ptr,
			      sizeof(bad_ptr)),
		   EFAULT);

	TEST_SUCC(setsockopt(sk, SOL_SOCKET, SO_ATTACH_FILTER, &prog,
			     sizeof(prog)));
	TEST_SUCC(setsockopt(sk, SOL_SOCKET, SO_DETACH_FILTER, &dummy,
			     sizeof(dummy)));
	TEST_ERRNO(setsockopt(sk, SOL_SOCKET, SO_DETACH_FILTER, &dummy,
			      sizeof(dummy)),
		   ENOENT);

	TEST_SUCC(close(sk));
}
END_TEST()

FN_TEST(tcp_attach_filter_unsupported)
{
	struct sock_filter accept_all[] = {
		BPF_STMT(BPF_RET | BPF_K, 0xFFFFFFFFU),
	};
	struct sock_fprog prog = {
		.len = 1,
		.filter = accept_all,
	};

	int tcp = TEST_SUCC(socket(AF_INET, SOCK_STREAM, 0));
	TEST_ERRNO(setsockopt(tcp, SOL_SOCKET, SO_ATTACH_FILTER, &prog,
			      sizeof(prog)),
		   ENOPROTOOPT);
	TEST_SUCC(close(tcp));
}
END_TEST()
