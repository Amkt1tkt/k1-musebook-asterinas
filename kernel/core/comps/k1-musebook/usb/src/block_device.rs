// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: aster-block front-end for USB MSC disks.

//! `aster-block` front-end for a USB MSC disk.
//!
//! `enqueue` completes I/O synchronously when its queue is empty.
//! The block registry also polls [`crate::poll_hotplug`] and spawns
//! a `handle_requests` kthread per disk so queued BIOs drain if a later
//! caller uses the queue.

use alloc::{string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};

use aster_block::{
    BlockDevice, BlockDeviceMeta, PartitionManager, SECTOR_SIZE,
    bio::{BioEnqueueError, BioSegment, BioStatus, BioType, SubmittedBio, bio_segment_pool_init},
    request_queue::{BioRequest, BioRequestSingleQueue},
};
use device_id::DeviceId;
use ostd::{
    mm::{HasDaddr, HasSize},
    sync::Mutex,
};
use spin::Once;

use crate::{
    msc::{self, MscDisk, SECTOR},
    xhci::{Error, Host},
};

static USB_MAJOR: Once<aster_block::MajorIdOwner> = Once::new();

fn major() -> device_id::MajorId {
    USB_MAJOR
        .call_once(|| aster_block::allocate_major().expect("usb blk major"))
        .get()
}

pub struct UsbBlockDevice {
    host: Arc<Mutex<Host>>,
    disk: MscDisk,
    queue: BioRequestSingleQueue,
    name: String,
    id: DeviceId,
    live: AtomicBool,
    partition_manager: PartitionManager,
}

impl core::fmt::Debug for UsbBlockDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UsbBlockDevice")
            .field("name", &self.name)
            .field("sectors", &self.disk.nsectors)
            .finish_non_exhaustive()
    }
}

impl BlockDevice for UsbBlockDevice {
    fn enqueue(&self, bio: SubmittedBio) -> Result<(), BioEnqueueError> {
        // Handle the BIO synchronously when the queue is empty. The registry
        // kthread drains queued requests through `handle_requests`.
        if self.queue.num_requests() == 0 {
            self.process_request(bio.into());
            Ok(())
        } else {
            self.queue.enqueue(bio)
        }
    }

    fn metadata(&self) -> BlockDeviceMeta {
        BlockDeviceMeta {
            max_nr_segments_per_bio: 16,
            nr_sectors: self.disk.nsectors as usize,
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn id(&self) -> DeviceId {
        self.id
    }

    fn partition_manager(&self) -> Option<&PartitionManager> {
        Some(&self.partition_manager)
    }
}

impl UsbBlockDevice {
    pub fn register(
        host: Arc<Mutex<Host>>,
        disk: MscDisk,
        name: &str,
        minor: u32,
    ) -> Result<Arc<Self>, Error> {
        let id = DeviceId::new(
            major(),
            device_id::MinorId::new(minor * aster_block::DEVICE_MINORS),
        );
        let dev = Arc::new(Self {
            host,
            disk,
            queue: BioRequestSingleQueue::new(),
            name: String::from(name),
            id,
            live: AtomicBool::new(true),
            partition_manager: PartitionManager::new(),
        });
        aster_block::register(dev.clone()).map_err(|_| Error::Protocol)?;
        bio_segment_pool_init();
        log::info!(
            "[k1-usb] registered {} sectors={} (MSC {:04x}:{:04x})",
            name,
            dev.disk.nsectors,
            dev.disk.vid,
            dev.disk.pid
        );
        Ok(dev)
    }

    pub fn mark_dead(&self) {
        self.live.store(false, Ordering::Release);
        self.partition_manager.clear();
        let _ = aster_block::unregister(self.id);
    }

    pub fn handle_requests(&self) {
        self.process_request(self.queue.dequeue());
    }

    // BioRequest applies the partition offset for both inline and queued I/O.
    fn process_request(&self, request: BioRequest) {
        match request.type_() {
            BioType::Read => self.io(request, true),
            BioType::Write => self.io(request, false),
            BioType::Flush => self.flush(request),
        }
    }

    fn io(&self, request: BioRequest, read: bool) {
        let mut lba = request.sid_range().start.to_raw();
        for bio in request.into_bios() {
            let sectors: u64 = bio
                .segments()
                .iter()
                .map(|segment| (segment.dma_slice().size() / SECTOR_SIZE) as u64)
                .sum();
            let result = if self.live.load(Ordering::Acquire) {
                transfer_segments(&bio, lba, |segment, sid| {
                    self.xfer_slice(segment, sid, read)
                })
            } else {
                Err(Error::ControllerFailed)
            };
            self.finish_bio(bio, result);
            lba += sectors;
        }
    }

    fn finish_bio(&self, bio: SubmittedBio, result: Result<(), Error>) {
        complete_or_retain(bio, result, |bio| self.host.lock().retain_bio(bio));
    }

    fn xfer_slice(&self, segment: &BioSegment, mut lba: u64, read: bool) -> Result<(), Error> {
        self.host.lock().check_running()?;
        let dma = segment.dma_slice();
        dma.mem_obj()
            .sync_to_device(dma.offset().clone())
            .map_err(|_| Error::Dma)?;
        let mut remaining = (dma.size() / SECTOR) as u64;
        let mut ptr = dma.daddr();
        if ptr
            .checked_add(dma.size())
            .is_none_or(|end| end > (1usize << 32))
            || lba
                .checked_add(remaining)
                .is_none_or(|end| end > self.disk.nsectors || end > u64::from(u32::MAX) + 1)
        {
            return Err(Error::Dma);
        }
        while remaining > 0 {
            let n = remaining.min(8); // 4 KiB bounce-friendly; TRB can do more
            let bytes = (n as u32) * SECTOR as u32;
            msc::xfer_daddr(
                &self.host,
                self.disk.slot,
                lba as u32,
                ptr as u64,
                bytes,
                read,
            )?;
            lba += n;
            remaining -= n;
            ptr += n as usize * SECTOR;
        }
        if read {
            dma.mem_obj()
                .sync_from_device(dma.offset().clone())
                .map_err(|_| Error::Dma)?;
        }
        Ok(())
    }

    fn flush(&self, request: BioRequest) {
        let mut result = self.flush_result();
        for bio in request.into_bios() {
            self.finish_bio(bio, result);
            // Only the first BIO represents this submitted command. Later
            // merged flushes have no DMA of their own and fail normally.
            if result == Err(Error::Timeout) {
                result = Err(Error::ControllerFailed);
            }
        }
    }

    fn flush_result(&self) -> Result<(), Error> {
        if !self.live.load(Ordering::Acquire) {
            return Err(Error::ControllerFailed);
        }
        msc::synchronize_cache(&self.host, self.disk.slot)
    }
}

/// Both synchronous and queued paths stop at the first failed segment. In
/// particular, no later buffer may be published after an unconfirmed timeout.
pub(crate) fn transfer_segments(
    bio: &SubmittedBio,
    mut lba: u64,
    mut transfer: impl FnMut(&BioSegment, u64) -> Result<(), Error>,
) -> Result<(), Error> {
    for segment in bio.segments() {
        transfer(segment, lba)?;
        lba += (segment.dma_slice().size() / SECTOR_SIZE) as u64;
    }
    Ok(())
}

/// A timeout does not establish DMA completion. Keep the whole request pending:
/// completing it would return its slices to their pools and wake buffer users.
/// ControllerFailed instead means this request never passed the submission gate.
pub(crate) fn complete_or_retain(
    bio: SubmittedBio,
    result: Result<(), Error>,
    retain: impl FnOnce(SubmittedBio),
) {
    match result {
        Err(Error::Timeout) => retain(bio),
        Ok(()) => bio.complete(BioStatus::Complete),
        Err(_) => bio.complete(BioStatus::IoError),
    }
}
