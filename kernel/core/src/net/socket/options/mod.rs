// SPDX-License-Identifier: MPL-2.0

use macros::impl_socket_options;

use super::util::{LingerOption, SocketTimeout};
use crate::{net::socket::unix::CUserCred, prelude::*, process::Gid, util::net::SockType};

pub(in crate::net) mod macros;

/// Socket options. This trait represents all options that can be set or got for a socket, including
/// socket level options and options for specific socket type like tcp socket.
pub(crate) trait SocketOption: Any + Send + Sync + Debug {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl_socket_options!(
    pub(crate) struct ReuseAddr(bool);
    pub(crate) struct SocketType(SockType);
    pub(crate) struct Error(Option<crate::error::Error>);
    pub(crate) struct Broadcast(bool);
    pub(crate) struct SendBuf(u32);
    pub(crate) struct RecvBuf(u32);
    pub(crate) struct KeepAlive(bool);
    pub(crate) struct Priority(i32);
    pub(crate) struct Linger(LingerOption);
    pub(crate) struct RecvTimeout(SocketTimeout);
    pub(crate) struct SendTimeout(SocketTimeout);
    pub(crate) struct ReusePort(bool);
    pub(crate) struct PassCred(bool);
    // K1 MUSE Book --- (journald treats a failing SO_TIMESTAMP as fatal; store the flag, no SCM_TIMESTAMP cmsg yet)
    /// `SO_TIMESTAMP` (`SOL_SOCKET`).
    ///
    /// Accepted and remembered; no `SCM_TIMESTAMP` control message is
    /// produced yet. journald treats a failing `setsockopt(SO_TIMESTAMP)`
    /// as fatal but only *uses* the cmsg when it happens to be present.
    pub(crate) struct Timestamp(bool);
    // K1 MUSE Book ---
    pub(crate) struct PeerCred(CUserCred);
    pub(crate) struct AcceptConn(bool);
    pub(crate) struct SendBufForce(u32);
    pub(crate) struct RecvBufForce(u32);
    pub(crate) struct PeerGroups(Arc<[Gid]>);
);

// K1 MUSE Book --- (libudev attaches a classic-BPF filter to its NETLINK_KOBJECT_UEVENT
//   monitor socket and treats setsockopt failure as fatal; without these two options
//   libinput never gets a seat and cosmic-comp has no input. Netlink-only no-op, see
//   `net::socket::netlink::common`.)
impl_socket_options!(
    /// `SO_ATTACH_FILTER` (`SOL_SOCKET`). Setter-only.
    ///
    /// Holds the instruction count from the userspace `struct sock_fprog`.
    /// `NETLINK_KOBJECT_UEVENT` accepts an unevaluated filter; other
    /// families return `ENOPROTOOPT`. See the netlink common-module docs.
    pub(crate) struct AttachFilter(u16);
    /// `SO_DETACH_FILTER` (`SOL_SOCKET`). Setter-only.
    ///
    /// Returns `ENOENT` if no filter is currently attached (Linux
    /// `sk_detach_filter`). Uevent-only; other protocols return `ENOPROTOOPT`.
    pub(crate) struct DetachFilter(());
);

/// Maximum number of classic BPF instructions.
///
/// Matches `BPF_MAXINSNS` in `include/uapi/linux/bpf_common.h`.
pub(crate) const BPF_MAXINSNS: u16 = 4096;

/// Validates the `len` field of a userspace `struct sock_fprog`.
///
/// Linux (`net/core/filter.c`, `bpf_check_classic`) rejects `len == 0` and
/// `len > BPF_MAXINSNS` with `EINVAL`.
pub(crate) fn validate_classic_bpf_len(len: u16) -> Result<()> {
    if len == 0 || len > BPF_MAXINSNS {
        return_errno_with_message!(Errno::EINVAL, "invalid classic BPF instruction count");
    }
    Ok(())
}
// K1 MUSE Book ---
