// SPDX-License-Identifier: MPL-2.0

use ostd::power::{ExitCode, poweroff, restart};

use super::SyscallReturn;
use crate::{
    prelude::*,
    process::{UserNamespace, credentials::capabilities::CapSet},
    security::lsm::hooks as lsm_hooks,
};

// Linux reboot magic constants.
const LINUX_REBOOT_MAGIC1: u32 = 0xfee1dead;
const LINUX_REBOOT_MAGIC2: u32 = 0x28121969;
const LINUX_REBOOT_MAGIC2A: u32 = 0x05121996;
const LINUX_REBOOT_MAGIC2B: u32 = 0x16041998;
const LINUX_REBOOT_MAGIC2C: u32 = 0x20112000;

/// Linux reboot commands.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, TryFromInt)]
enum RebootCmd {
    Restart = 0x01234567,
    Halt = 0xcdef0123,
    PowerOff = 0x4321fedc,
    // TODO: Add more reboot sub-commands.
}

pub(super) fn sys_reboot(
    magic1: u32,
    magic2: u32,
    op: u32,
    _arg: Vaddr,
    ctx: &Context,
) -> Result<SyscallReturn> {
    debug!(
        "[sys_reboot]: magic1 = {:#x}, magic2 = {:#x}, op = {:#x}",
        magic1, magic2, op
    );

    // Verify magic numbers
    if magic1 != LINUX_REBOOT_MAGIC1 {
        return_errno_with_message!(Errno::EINVAL, "the reboot magic is invalid");
    }
    if magic2 != LINUX_REBOOT_MAGIC2
        && magic2 != LINUX_REBOOT_MAGIC2A
        && magic2 != LINUX_REBOOT_MAGIC2B
        && magic2 != LINUX_REBOOT_MAGIC2C
    {
        return_errno_with_message!(Errno::EINVAL, "the reboot magic is invalid");
    }

    lsm_hooks::on_capable(lsm_hooks::CapableContext::new(
        UserNamespace::get_init_singleton().as_ref(),
        ctx.posix_thread,
        CapSet::SYS_BOOT,
    ))?;

    let cmd = RebootCmd::try_from(op)?;

    // K1 MUSE Book --- (shutdown errors must not cut power or resume writers after VALID)
    // TODO: Perform any necessary cleanup before powering off or restarting.
    // No filesystem locks are held while waiting for a running flusher.
    // Keep this guard in function scope until the final non-returning action.
    let _writeback_guard = crate::fs::writeback::pause_for_shutdown();
    {
        let current_ns_proxy = ctx.thread_local.borrow_ns_proxy();
        let current_ns = current_ns_proxy.unwrap().mnt_ns();
        let init_ns = crate::fs::vfs::path::MountNamespace::get_init_singleton();
        // Include the initial namespace even if the caller has a private mount
        // namespace. A failed normal sync has not marked any filesystem clean.
        init_ns.sync()?;
        if !Arc::ptr_eq(current_ns, init_ns) {
            current_ns.sync()?;
        }
        let result = init_ns.sync_for_shutdown().and_then(|()| {
            if Arc::ptr_eq(current_ns, init_ns) {
                Ok(())
            } else {
                current_ns.sync_for_shutdown()
            }
        });
        if let Err(err) = result {
            error!(
                "[shutdown] clean/flush failed: {:?}; halting with power retained (do not resume writes)",
                err
            );
            ostd::power::halt();
        }

        #[cfg(target_arch = "riscv64")]
        if k1_musebook_i2c::k1_pmic_power::is_supported() {
            if cmd == RebootCmd::Halt {
                ostd::power::halt();
            }
            if let Err(err) = k1_musebook_i2c::k1_pmic_power::request(cmd == RebootCmd::Restart) {
                error!("[shutdown] {}; halting with power retained", err);
            }
            ostd::power::halt();
        }
    }
    // K1 MUSE Book ---
    match cmd {
        RebootCmd::Restart => restart(ExitCode::Success),
        RebootCmd::Halt | RebootCmd::PowerOff => poweroff(ExitCode::Success),
    }
}
