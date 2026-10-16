// SPDX-License-Identifier: MPL-2.0
// K1 MUSE Book: retain published xHCI DMA and an uncompleted BIO after timeout.

use core::{mem, ops::Deref};

use super::Error;

/// A hardware-visible allocation must outlive its owner until the controller
/// is confirmed stopped. This driver has no shutdown/recovery path yet.
pub(crate) struct DmaRetention<T> {
    owner: Option<T>,
    published: bool,
}

impl<T> DmaRetention<T> {
    pub(crate) fn new(owner: T) -> Self {
        Self {
            owner: Some(owner),
            published: false,
        }
    }

    pub(crate) fn publish(&mut self) {
        self.published = true;
    }
}

impl<T> Deref for DmaRetention<T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.owner.as_ref().expect("DMA owner exists until drop")
    }
}

impl<T> Drop for DmaRetention<T> {
    fn drop(&mut self) {
        if self.published {
            // One controller allocation, including rings, contexts and bounce
            // pages. Do not let init failure or removal free active DMA.
            mem::forget(self.owner.take());
        }
    }
}

/// Serialized by the host lock: at most one synchronous BOT command can time
/// out. Later calls fail before submission and cannot add a second owner.
pub(crate) struct FailureState<T> {
    failed: bool,
    pending: Option<T>,
}

impl<T> FailureState<T> {
    pub(crate) const fn new() -> Self {
        Self {
            failed: false,
            pending: None,
        }
    }

    pub(crate) fn check_running(&self) -> Result<(), Error> {
        if self.failed {
            Err(Error::ControllerFailed)
        } else {
            Ok(())
        }
    }

    pub(crate) fn fail(&mut self) {
        self.failed = true;
    }

    pub(crate) fn retain(&mut self, owner: T) {
        assert!(self.failed, "only an unconfirmed timeout retains a BIO");
        assert!(
            self.pending.is_none(),
            "failed host cannot submit another BIO"
        );
        self.pending = Some(owner);
    }

    #[cfg(ktest)]
    pub(crate) fn retained(&self) -> Option<&T> {
        self.pending.as_ref()
    }

    #[cfg(ktest)]
    pub(crate) fn take_retained_for_test(&mut self) -> Option<T> {
        self.pending.take()
    }
}

impl<T> Drop for FailureState<T> {
    fn drop(&mut self) {
        // Keep SubmittedBio itself: an Arc<DmaStream> alone would not stop its
        // segments returning to the pool or its caller reusing completed I/O.
        mem::forget(self.pending.take());
    }
}
