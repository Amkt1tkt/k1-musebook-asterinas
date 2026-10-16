// SPDX-License-Identifier: MPL-2.0

use aster_bigtcp::socket::ReceiveBehavior;
use bitflags::bitflags;

bitflags! {
    /// Flags passed to socket send operations.
    #[repr(C)]
    #[derive(Pod)]
    pub(crate) struct SendFlags: i32 {
        const MSG_OOB       = 0x1;
        const MSG_DONTROUTE = 0x4;
        const MSG_PROBE     = 0x10;
        const MSG_DONTWAIT  = 0x40;
        const MSG_EOR       = 0x80;
        const MSG_FIN       = 0x200;
        const MSG_SYN       = 0x400;
        const MSG_CONFIRM   = 0x800;
        const MSG_RST       = 0x1000;
        const MSG_NOSIGNAL  = 0x4000;
        const MSG_MORE      = 0x8000;
        const MSG_BATCH     = 0x40000;
        const MSG_ZEROCOPY  = 0x4000000;
        const MSG_FASTOPEN  = 0x20000000;

        // K1 MUSE Book --- (MSG_DONTWAIT is honored by every send path via `block_on_unless`;
        // MSG_NOSIGNAL is a no-op because no send path raises SIGPIPE on EPIPE yet, so
        // its semantics hold by construction)
        //         const SUPPORTED     = 0x0;
        const SUPPORTED     = Self::MSG_DONTWAIT.bits | Self::MSG_NOSIGNAL.bits;
        // K1 MUSE Book ---
    }
}

impl SendFlags {
    pub(crate) const fn is_all_supported(self) -> bool {
        Self::SUPPORTED.contains(self)
    }

    // K1 MUSE Book --- (MSG_DONTWAIT)
    /// Whether this operation requests nonblocking I/O independently of the fd.
    pub(crate) const fn dont_wait(self) -> bool {
        self.contains(Self::MSG_DONTWAIT)
    }
    // K1 MUSE Book ---
}

bitflags! {
    /// Flags passed to or returned from socket receive operations.
    #[repr(C)]
    #[derive(Pod)]
    pub(crate) struct RecvFlags: i32 {
        const MSG_OOB          = SendFlags::MSG_OOB.bits;
        const MSG_PEEK         = 0x2;
        const MSG_CTRUNC       = 0x8;
        const MSG_TRUNC        = 0x20;
        const MSG_DONTWAIT     = SendFlags::MSG_DONTWAIT.bits;
        const MSG_EOR          = SendFlags::MSG_EOR.bits;
        const MSG_WAITALL      = 0x100;
        const MSG_ERRQUEUE     = 0x2000;
        const MSG_WAITFORONE   = 0x10000;
        const MSG_SOCK_DEVMEM  = 0x2000000;
        const MSG_CMSG_CLOEXEC = 0x40000000;

        // K1 MUSE Book --- (MSG_DONTWAIT is honored by every receive path via `block_on_unless`;
        // MSG_CMSG_CLOEXEC is honored when SCM_RIGHTS fds are installed, see `fd_flags`)
        //         const SUPPORTED        = RecvFlags::MSG_PEEK.bits | RecvFlags::MSG_TRUNC.bits;
        const SUPPORTED        = RecvFlags::MSG_PEEK.bits | RecvFlags::MSG_TRUNC.bits | RecvFlags::MSG_DONTWAIT.bits | RecvFlags::MSG_CMSG_CLOEXEC.bits;
        // K1 MUSE Book ---
    }
}

impl RecvFlags {
    pub(crate) const fn is_all_supported(self) -> bool {
        Self::SUPPORTED.contains(self)
    }

    // K1 MUSE Book --- (MSG_DONTWAIT)
    /// Whether this operation requests nonblocking I/O independently of the fd.
    pub(crate) const fn dont_wait(self) -> bool {
        self.contains(Self::MSG_DONTWAIT)
    }
    // K1 MUSE Book ---

    // K1 MUSE Book --- (MSG_CMSG_CLOEXEC)
    /// The `FdFlags` for file descriptors installed from a received
    /// `SCM_RIGHTS` message: `CLOEXEC` iff `MSG_CMSG_CLOEXEC` was passed.
    pub(crate) fn fd_flags(self) -> crate::fs::file::file_table::FdFlags {
        use crate::fs::file::file_table::FdFlags;
        if self.contains(Self::MSG_CMSG_CLOEXEC) {
            FdFlags::CLOEXEC
        } else {
            FdFlags::empty()
        }
    }
    // K1 MUSE Book ---

    /// Returns whether receive operations should consume or peek data.
    pub(crate) fn receive_behavior(self) -> ReceiveBehavior {
        if self.contains(Self::MSG_PEEK) {
            ReceiveBehavior::Peek
        } else {
            ReceiveBehavior::Recv
        }
    }
}
