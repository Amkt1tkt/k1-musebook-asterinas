// SPDX-License-Identifier: MPL-2.0

#![short_vis_path::add(vmar)]

use super::{Interval, RssDelta, Vmar};
use crate::{prelude::*, vm::perms::VmPerms};

impl Vmar {
    // K1 MUSE Book --- (GEM mmap failures carry a signal verdict, not only an errno)
    // pub(crate) fn handle_page_fault(&self, page_fault_info: &PageFaultInfo) -> Result<()> {
    //     let inner = self.inner.read();
    //
    //     let address = page_fault_info.address;
    //     if let Some(vm_mapping) = inner.vm_mappings.find_one(&address) {
    //         debug_assert!(vm_mapping.range().contains(&address));
    //
    //         let mut rss_delta = RssDelta::new(self);
    //         return vm_mapping.handle_page_fault(&self.vm_space, page_fault_info, &mut rss_delta);
    //     }
    //
    //     return_errno_with_message!(
    //         Errno::EACCES,
    //         "no VM mappings contain the page fault address"
    //     );
    // }
    pub(crate) fn handle_page_fault(
        &self,
        page_fault_info: &PageFaultInfo,
    ) -> core::result::Result<(), PageFaultFailure> {
        let inner = self.inner.read();

        let address = page_fault_info.address;
        if let Some(vm_mapping) = inner.vm_mappings.find_one(&address) {
            debug_assert!(vm_mapping.range().contains(&address));

            let mut rss_delta = RssDelta::new(self);
            return vm_mapping.handle_page_fault(&self.vm_space, page_fault_info, &mut rss_delta);
        }

        Err(Error::with_message(
            Errno::EACCES,
            "no VM mappings contain the page fault address",
        )
        .into())
    }
    // K1 MUSE Book ---
}

/// Page fault information converted from [`CpuException`].
///
/// `TryFrom<CpuException>` should be implemented for this struct.
/// If [`CpuException`] is a page fault, `try_from` should return `Ok(PageFaultInfo)`,
/// or `Err(())` (no error information) otherwise.
///
/// [`CpuException`]: ostd::arch::cpu::context::CpuException
#[derive(Debug)]
pub(crate) struct PageFaultInfo {
    /// The virtual address where a page fault occurred.
    pub(in vmar) address: Vaddr,

    /// The [`VmPerms`] required by the memory operation that causes page fault.
    /// For example, a "store" operation may require `VmPerms::WRITE`.
    pub(in vmar) required_perms: VmPerms,

    /// Whether this page fault is forced (e.g., manually triggered by `ptrace`).
    /// A forced page fault may bypass some permission checks.
    is_forced: bool,
}

impl PageFaultInfo {
    /// Creates a new `PageFaultInfo`.
    pub(crate) fn new(address: Vaddr, required_perms: VmPerms) -> Self {
        Self {
            address,
            required_perms,
            is_forced: false,
        }
    }

    /// Returns whether this page fault is forced.
    pub(in vmar) fn is_forced(&self) -> bool {
        self.is_forced
    }

    /// Marks this page fault as forced.
    pub(super) fn force(mut self) -> Self {
        self.is_forced = true;
        self
    }

    // K1 MUSE Book --- (exception.rs is outside `vmar` and needs the fault VA for SIGBUS)
    pub(crate) fn address(&self) -> Vaddr {
        self.address
    }
    // K1 MUSE Book ---
}

// K1 MUSE Book --- (refused GEM pages are SIGBUS; other faults stay with the exception)
/// A page fault that could not be resolved.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageFaultFailure {
    error: Error,
    signal: PageFaultSignal,
}

impl PageFaultFailure {
    pub(in crate::vm) fn new(error: Error, signal: PageFaultSignal) -> Self {
        Self { error, signal }
    }

    #[cfg_attr(not(ktest), expect(dead_code))]
    pub(crate) fn error(&self) -> Error {
        self.error
    }

    pub(crate) fn signal(&self) -> PageFaultSignal {
        self.signal
    }
}

impl From<Error> for PageFaultFailure {
    fn from(error: Error) -> Self {
        Self::new(error, PageFaultSignal::FromException)
    }
}

impl From<ostd::Error> for PageFaultFailure {
    fn from(error: ostd::Error) -> Self {
        Error::from(error).into()
    }
}

impl From<PageFaultFailure> for Error {
    fn from(failure: PageFaultFailure) -> Self {
        failure.error
    }
}

/// Which signal an unresolvable page fault must be reported to user space with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PageFaultSignal {
    /// Whatever the CPU exception implies (`SIGSEGV` for a page fault).
    FromException,
    /// `SIGBUS`: the address is mapped, but the page has no backing store.
    Bus,
}
// K1 MUSE Book ---
