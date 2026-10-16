// SPDX-License-Identifier: MPL-2.0

#[cfg(not(target_arch = "loongarch64"))]
use ostd::arch::cpu::context::CpuException;
#[cfg(target_arch = "loongarch64")]
use ostd::arch::cpu::context::CpuExceptionInfo as CpuException;
use ostd::{arch::cpu::context::UserContext, task::Task};

// K1 MUSE Book --- (refused GEM pages are SIGBUS)
use crate::vm::vmar::{PageFaultFailure, PageFaultSignal};
use crate::{
    prelude::*,
    process::signal::signals::fault::FaultSignal,
    thread::kernel_thread::AsKernelThread,
    vm::vmar::{PageFaultInfo, Vmar},
};
// K1 MUSE Book ---

pub(super) fn handle_exception(ctx: &Context, user_ctx: &UserContext, exception: CpuException) {
    debug!("handle exception: {:#x?}", exception);

    if let Ok(page_fault_info) = PageFaultInfo::try_from(&exception) {
        let user_space = ctx.user_space();
        let vmar = user_space.vmar();
        // K1 MUSE Book --- (refused GEM pages are SIGBUS, not the exception's SIGSEGV)
        // if handle_page_fault_from_vmar(vmar, &page_fault_info).is_ok() {
        //     return;
        // }
        match handle_page_fault_from_vmar(vmar, &page_fault_info) {
            Ok(()) => return,
            Err(failure) => {
                if let Some(signal) = page_fault_signal(failure.signal(), page_fault_info.address())
                {
                    ctx.posix_thread.enqueue_signal(Box::new(signal));
                    return;
                }
            }
        }
        // K1 MUSE Book ---
    }

    // We cannot handle most exceptions. Send a fault signal to the current thread before returning
    // to user space.
    generate_fault_signal(exception, ctx, user_ctx);
}

/// Handles the page fault occurs in the VMAR.
// K1 MUSE Book --- (propagate the signal verdict so GEM refusals become SIGBUS)
// fn handle_page_fault_from_vmar(vmar: &Vmar, page_fault_info: &PageFaultInfo) -> Result<(), ()> {
//     if let Err(e) = vmar.handle_page_fault(page_fault_info) {
//         warn!(
//             "page fault handler failed: info: {:#x?}, err: {:?}",
//             page_fault_info, e
//         );
//         return Err(());
//     }
//     Ok(())
// }
fn handle_page_fault_from_vmar(
    vmar: &Vmar,
    page_fault_info: &PageFaultInfo,
) -> core::result::Result<(), PageFaultFailure> {
    if let Err(e) = vmar.handle_page_fault(page_fault_info) {
        warn!(
            "page fault handler failed: info: {:#x?}, err: {:?}",
            page_fault_info, e
        );
        return Err(e);
    }
    Ok(())
}

/// Converts the verdict of the page fault path into a signal.
fn page_fault_signal(verdict: PageFaultSignal, address: Vaddr) -> Option<FaultSignal> {
    use crate::process::signal::constants::{BUS_ADRERR, SIGBUS};

    match verdict {
        PageFaultSignal::FromException => None,
        PageFaultSignal::Bus => Some(FaultSignal::new(SIGBUS, BUS_ADRERR, Some(address as u64))),
    }
}
// K1 MUSE Book ---

/// A trait that converts CPU exceptions into fault signals.
///
/// This trait should be implemented by architecture-specific code for [`CpuException`].
pub(crate) trait ToFaultSignal {
    /// Converts a CPU exception into a fault signal.
    ///
    /// Returns `None` if the exception must be handled earlier and cannot be delivered as a signal.
    ///
    /// POSIX [requires] `SIGILL` and `SIGFPE` to report the address of the faulting instruction,
    /// and `SIGSEGV` and `SIGBUS` to report the address of the faulting memory reference. Linux
    /// behavior, however, is highly architecture-specific and does not always follow POSIX: for
    /// some exceptions, it reports neither the expected code nor the expected address. Linux may
    /// also attach an instruction address to `SIGTRAP`, but this behavior is not consistent across
    /// architectures.
    ///
    /// [requires]: https://pubs.opengroup.org/onlinepubs/009695399/basedefs/signal.h.html
    fn to_fault_signal(&self, user_ctx: &UserContext) -> Option<FaultSignal>;
}

/// Generates a fault signal for the current thread.
fn generate_fault_signal(exception: CpuException, ctx: &Context, user_ctx: &UserContext) {
    let Some(signal) = exception.to_fault_signal(user_ctx) else {
        panic!("`{:?}` cannot be handled via signals", exception);
    };
    ctx.posix_thread.enqueue_signal(Box::new(signal));
}

pub(super) fn page_fault_handler(info: &CpuException) -> Result<(), ()> {
    let task = Task::current().unwrap();
    let page_fault_info = info.try_into().unwrap();

    if let Some(thread_local) = task.as_thread_local() {
        if thread_local.is_page_fault_disabled() {
            // Do nothing if the page fault handler is disabled. This will typically cause the
            // fallible memory operation to report EFAULT errors immediately.
            return Err(());
        }

        let user_space = CurrentUserSpace::new(thread_local);
        // K1 MUSE Book --- (in-kernel EFAULT path does not deliver a signal)
        // handle_page_fault_from_vmar(user_space.vmar(), &page_fault_info)
        handle_page_fault_from_vmar(user_space.vmar(), &page_fault_info).map_err(|_| ())
        // K1 MUSE Book ---
    } else {
        let vmar = task
            .as_kernel_thread()
            .and_then(|kernel_thread| kernel_thread.vmar())
            .ok_or(())?;
        // K1 MUSE Book --- (in-kernel EFAULT path does not deliver a signal)
        // handle_page_fault_from_vmar(vmar, &page_fault_info)
        handle_page_fault_from_vmar(vmar, &page_fault_info).map_err(|_| ())
        // K1 MUSE Book ---
    }
}

// K1 MUSE Book --- (SIGBUS conversion for a refused device page)
#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;
    use crate::{
        process::signal::constants::{BUS_ADRERR, SIGBUS},
        vm::vmar::on_demand::DeviceFaultError,
    };

    const FAULT_ADDR: Vaddr = 0x1_0000_0123;

    #[ktest]
    fn a_refused_device_page_is_a_sigbus() {
        let expected = FaultSignal::new(SIGBUS, BUS_ADRERR, Some(FAULT_ADDR as u64));

        for error in [DeviceFaultError::OutOfBounds, DeviceFaultError::OutOfMemory] {
            let failure = PageFaultFailure::from(error);

            assert_eq!(failure.signal(), PageFaultSignal::Bus);
            assert_eq!(
                page_fault_signal(failure.signal(), FAULT_ADDR),
                Some(expected),
                "{:?} must be reported as SIGBUS",
                error
            );
            assert_eq!(failure.error().error(), error.errno());
        }

        assert_ne!(
            DeviceFaultError::OutOfBounds.errno(),
            DeviceFaultError::OutOfMemory.errno()
        );
    }

    #[ktest]
    fn other_failures_leave_the_signal_to_the_exception() {
        let failure = PageFaultFailure::from(Error::new(Errno::EACCES));

        assert_eq!(failure.signal(), PageFaultSignal::FromException);
        assert_eq!(page_fault_signal(failure.signal(), FAULT_ADDR), None);
    }
}
// K1 MUSE Book ---
