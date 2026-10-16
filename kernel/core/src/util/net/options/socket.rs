// SPDX-License-Identifier: MPL-2.0

use ostd::mm::VmIo;

use super::{RawSocketOption, impl_raw_sock_option_get_only, impl_raw_socket_option};
// K1 MUSE Book --- (SO_ATTACH_FILTER / SO_DETACH_FILTER)
use crate::net::socket::options::{AttachFilter, DetachFilter, validate_classic_bpf_len};
// K1 MUSE Book ---
// K1 MUSE Book --- (expose desktop socket-option compatibility types)
//         SendTimeout, SocketOption, SocketType,
use crate::{
    context::current_userspace,
    net::socket::options::{
        AcceptConn, Broadcast, Error, KeepAlive, Linger, PassCred, PeerCred, PeerGroups, Priority,
        RecvBuf, RecvBufForce, RecvTimeout, ReuseAddr, ReusePort, SendBuf, SendBufForce,
        SendTimeout, SocketOption, SocketType, Timestamp,
    },
    prelude::*,
    process::Gid,
};
// K1 MUSE Book ---

/// Socket level options.
///
/// The definition is from <https://elixir.bootlin.com/linux/v6.0.9/source/include/uapi/asm-generic/socket.h>.
#[expect(non_camel_case_types)]
#[expect(clippy::upper_case_acronyms)]
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, TryFromInt)]
enum CSocketOptionName {
    DEBUG = 1,
    REUSEADDR = 2,
    TYPE = 3,
    ERROR = 4,
    DONTROUTE = 5,
    BROADCAST = 6,
    SNDBUF = 7,
    RCVBUF = 8,
    KEEPALIVE = 9,
    OOBINLINE = 10,
    NO_CHECK = 11,
    PRIORITY = 12,
    LINGER = 13,
    BSDCOMPAT = 14,
    REUSEPORT = 15,
    PASSCRED = 16,
    PEERCRED = 17,
    RCVTIMEO_OLD = 20,
    SNDTIMEO_OLD = 21,
    ATTACH_FILTER = 26,
    DETACH_FILTER = 27,
    // K1 MUSE Book --- (SO_TIMESTAMP; journald requires the setsockopt to succeed)
    TIMESTAMP_OLD = 29,
    // K1 MUSE Book ---
    ACCPETCONN = 30,
    PEERSEC = 31,
    SNDBUFFORCE = 32,
    RCVBUFFORCE = 33,
    PEERGROUPS = 59,
    RCVTIMEO_NEW = 66,
    SNDTIMEO_NEW = 67,
}

pub(crate) fn new_socket_option(name: i32) -> Result<Box<dyn RawSocketOption>> {
    let name = CSocketOptionName::try_from(name).map_err(|_| Errno::ENOPROTOOPT)?;
    match name {
        CSocketOptionName::REUSEADDR => Ok(Box::new(ReuseAddr::new())),
        CSocketOptionName::TYPE => Ok(Box::new(SocketType::new())),
        CSocketOptionName::ERROR => Ok(Box::new(Error::new())),
        CSocketOptionName::BROADCAST => Ok(Box::new(Broadcast::new())),
        CSocketOptionName::SNDBUF => Ok(Box::new(SendBuf::new())),
        CSocketOptionName::RCVBUF => Ok(Box::new(RecvBuf::new())),
        CSocketOptionName::KEEPALIVE => Ok(Box::new(KeepAlive::new())),
        CSocketOptionName::PRIORITY => Ok(Box::new(Priority::new())),
        CSocketOptionName::LINGER => Ok(Box::new(Linger::new())),
        // On 64-bit systems, the old and new timeout options share the same
        // `timeval_t` layout and can use the same handlers. A 32-bit userspace
        // ABI would need separate handlers for the old and new layouts.
        CSocketOptionName::RCVTIMEO_OLD | CSocketOptionName::RCVTIMEO_NEW => {
            Ok(Box::new(RecvTimeout::new()))
        }
        CSocketOptionName::SNDTIMEO_OLD | CSocketOptionName::SNDTIMEO_NEW => {
            Ok(Box::new(SendTimeout::new()))
        }
        CSocketOptionName::REUSEPORT => Ok(Box::new(ReusePort::new())),
        CSocketOptionName::PASSCRED => Ok(Box::new(PassCred::new())),
        CSocketOptionName::PEERCRED => Ok(Box::new(PeerCred::new())),
        // K1 MUSE Book --- (accept SO_TIMESTAMP; stored, no cmsg)
        CSocketOptionName::TIMESTAMP_OLD => Ok(Box::new(Timestamp::new())),
        // K1 MUSE Book ---
        // K1 MUSE Book --- (produced here because they are `SOL_SOCKET` names; only netlink
        //   sockets handle them, as an unevaluated no-op, other families still get
        //   `ENOPROTOOPT` from `SocketOptionSet`)
        CSocketOptionName::ATTACH_FILTER => Ok(Box::new(AttachFilter::new())),
        CSocketOptionName::DETACH_FILTER => Ok(Box::new(DetachFilter::new())),
        // K1 MUSE Book ---
        CSocketOptionName::ACCPETCONN => Ok(Box::new(AcceptConn::new())),
        CSocketOptionName::SNDBUFFORCE => Ok(Box::new(SendBufForce::new())),
        CSocketOptionName::RCVBUFFORCE => Ok(Box::new(RecvBufForce::new())),
        CSocketOptionName::PEERGROUPS => Ok(Box::new(PeerGroups::new())),
        _ => return_errno_with_message!(Errno::ENOPROTOOPT, "unsupported socket-level option"),
    }
}

impl_raw_socket_option!(ReuseAddr);
impl_raw_sock_option_get_only!(SocketType);
impl_raw_sock_option_get_only!(Error);
impl_raw_socket_option!(Broadcast);
impl_raw_socket_option!(SendBuf);
impl_raw_socket_option!(RecvBuf);
impl_raw_socket_option!(KeepAlive);
impl_raw_socket_option!(Priority);
impl_raw_socket_option!(Linger);
impl_raw_socket_option!(RecvTimeout);
impl_raw_socket_option!(SendTimeout);
impl_raw_socket_option!(ReusePort);
impl_raw_socket_option!(PassCred);
// K1 MUSE Book --- (SO_TIMESTAMP get/set)
impl_raw_socket_option!(Timestamp);
// K1 MUSE Book ---
impl_raw_sock_option_get_only!(PeerCred);
impl_raw_sock_option_get_only!(AcceptConn);
impl_raw_socket_option!(SendBufForce);
impl_raw_socket_option!(RecvBufForce);

// SO_PEERGROUPS is a read-only option. However, calling setsockopt on SO_PEERGROUPS will return EINVAL
// instead of ENOPROTOOPT like other options. Therefore, we manually implement `RawSocketOption` for it.
impl RawSocketOption for PeerGroups {
    fn read_from_user(&mut self, _addr: Vaddr, _max_len: u32) -> Result<()> {
        return_errno_with_message!(Errno::EINVAL, "the option is getter-only");
    }

    fn write_to_user(&self, addr: Vaddr, buffer_len: &mut u32) -> Result<usize> {
        let groups = self.get().unwrap();

        let old_len = *buffer_len;
        *buffer_len = (groups.len() * size_of::<Gid>()) as u32;
        if old_len < *buffer_len {
            return_errno_with_message!(Errno::ERANGE, "the buffer is too small");
        }

        for (i, gid) in groups.iter().enumerate() {
            let dst = addr + i * size_of::<Gid>();
            current_userspace!().write_val(dst, gid)?;
        }

        Ok(*buffer_len as usize)
    }

    fn as_sock_option_mut(&mut self) -> &mut dyn SocketOption {
        self
    }

    fn as_sock_option(&self) -> &dyn SocketOption {
        self
    }
}

// K1 MUSE Book --- (decode the userspace `struct sock_fprog` for SO_ATTACH_FILTER so that
//   bad pointers yield EFAULT and bad lengths EINVAL; the instructions
//   themselves are discarded because no socket family evaluates cBPF in this port)
/// Userspace `struct sock_filter` (`include/uapi/linux/filter.h`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct CSockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// Userspace `struct sock_fprog` on LP64 (`include/uapi/linux/filter.h`).
///
/// Layout is 16 bytes with the filter pointer at offset 8.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod)]
struct CSockFprog {
    len: u16,
    _pad: [u8; 6],
    filter: Vaddr,
}

const _: () = {
    assert!(size_of::<CSockFilter>() == 8);
    assert!(size_of::<CSockFprog>() == 16);
    assert!(core::mem::offset_of!(CSockFprog, filter) == 8);
};

/// Reads a userspace `struct sock_fprog` and copies the filter program so a
/// bad pointer yields `EFAULT`. The instructions themselves are discarded.
fn read_sock_fprog_from_user(addr: Vaddr, max_len: u32) -> Result<u16> {
    // Linux sk_setsockopt checks and copies an int before filter-specific
    // optlen validation. Preserve EFAULT precedence for inaccessible optval.
    if (max_len as usize) < size_of::<i32>() {
        return_errno_with_message!(Errno::EINVAL, "the socket option is shorter than an int");
    }
    current_userspace!().read_val::<i32>(addr)?;
    if (max_len as usize) != size_of::<CSockFprog>() {
        return_errno_with_message!(Errno::EINVAL, "optlen must equal struct sock_fprog size");
    }

    let fprog = current_userspace!().read_val::<CSockFprog>(addr)?;
    validate_classic_bpf_len(fprog.len)?;

    let nbytes = (fprog.len as usize) * size_of::<CSockFilter>();
    let mut buf = vec![0u8; nbytes];
    current_userspace!().read_bytes(fprog.filter, &mut buf)?;
    Ok(fprog.len)
}

impl RawSocketOption for AttachFilter {
    fn read_from_user(&mut self, addr: Vaddr, max_len: u32) -> Result<()> {
        let len = read_sock_fprog_from_user(addr, max_len)?;
        self.set(len);
        Ok(())
    }

    fn write_to_user(&self, _addr: Vaddr, _max_len: &mut u32) -> Result<usize> {
        return_errno_with_message!(Errno::ENOPROTOOPT, "the option is setter-only");
    }

    fn as_sock_option_mut(&mut self) -> &mut dyn SocketOption {
        self
    }

    fn as_sock_option(&self) -> &dyn SocketOption {
        self
    }
}

impl RawSocketOption for DetachFilter {
    fn read_from_user(&mut self, addr: Vaddr, max_len: u32) -> Result<()> {
        // sk_detach_filter ignores the value, but sk_setsockopt first copies it.
        if (max_len as usize) < size_of::<i32>() {
            return_errno_with_message!(Errno::EINVAL, "the socket option is shorter than an int");
        }
        current_userspace!().read_val::<i32>(addr)?;
        self.set(());
        Ok(())
    }

    fn write_to_user(&self, _addr: Vaddr, _max_len: &mut u32) -> Result<usize> {
        return_errno_with_message!(Errno::ENOPROTOOPT, "the option is setter-only");
    }

    fn as_sock_option_mut(&mut self) -> &mut dyn SocketOption {
        self
    }

    fn as_sock_option(&self) -> &dyn SocketOption {
        self
    }
}
// K1 MUSE Book ---
