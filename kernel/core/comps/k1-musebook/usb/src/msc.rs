// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: USB mass-storage BOT / SCSI command path.

//! USB Mass Storage Bulk-Only Transport + SCSI (INQUIRY / CAPACITY / R/W10).

use core::sync::atomic::{AtomicU32, Ordering};

use ostd::sync::Mutex;

use crate::{
    scsi::{
        CSW_LEN, Csw, cdb_inquiry, cdb_read_capacity10, cdb_read10, cdb_request_sense,
        cdb_synchronize_cache10, cdb_test_unit_ready, cdb_write10, encode_cbw, parse_capacity10,
    },
    xhci::{Error, Host},
};

pub(crate) const SECTOR: usize = 512;

static TAG: AtomicU32 = AtomicU32::new(1);

pub struct MscDisk {
    pub slot: u8,
    pub nsectors: u64,
    pub vid: u16,
    pub pid: u16,
}

pub(crate) fn bringup(host: &Mutex<Host>, slot: u8, vid: u16, pid: u16) -> Result<MscDisk, Error> {
    let mut inq = [0u8; 36];
    bot(host, slot, &cdb_inquiry(36), Some(&mut inq), true)?;
    let vendor = core::str::from_utf8(&inq[8..16]).unwrap_or("????");
    let product = core::str::from_utf8(&inq[16..32]).unwrap_or("????");
    log::info!(
        "[k1-usb] msc inquiry slot{} '{}' '{}'",
        slot,
        vendor.trim(),
        product.trim()
    );

    // Some sticks need TUR + REQUEST SENSE after reset (UNIT ATTENTION).
    for _ in 0..3 {
        match bot(host, slot, &cdb_test_unit_ready(), None, false) {
            Ok(()) => break,
            Err(Error::Stall) | Err(Error::Protocol) => {
                let mut sense = [0u8; 18];
                let _ = bot(host, slot, &cdb_request_sense(18), Some(&mut sense), true);
            }
            Err(e) => return Err(e),
        }
    }

    let mut cap = [0u8; 8];
    bot(host, slot, &cdb_read_capacity10(), Some(&mut cap), true)?;
    let Some((last, blen)) = parse_capacity10(&cap) else {
        log::warn!("[k1-usb] msc READ CAPACITY parse fail {:02x?}", cap);
        return Err(Error::Protocol);
    };
    if last == u32::MAX {
        // SBC READ CAPACITY(10) uses this sentinel to require CAPACITY(16).
        // This driver only implements 32-bit READ/WRITE(10).
        return Err(Error::Protocol);
    }
    if blen != SECTOR as u32 {
        // The block adapter and READ/WRITE(10) path use 512-byte sectors.
        log::warn!(
            "[k1-usb] unsupported MSC block length {} (requires 512)",
            blen
        );
        return Err(Error::Protocol);
    }
    let nsectors = last as u64 + 1;
    log::info!(
        "[k1-usb] msc capacity slot{} last_lba={:#x} nsectors={} blen={}",
        slot,
        last,
        nsectors,
        blen
    );
    Ok(MscDisk {
        slot,
        nsectors,
        vid,
        pid,
    })
}

/// Complete a whole-device cache synchronization before reporting BIO Flush.
/// IMMED=0 waits for completion; unsupported commands remain an I/O error.
pub(crate) fn synchronize_cache(host: &Mutex<Host>, slot: u8) -> Result<(), Error> {
    bot(host, slot, &cdb_synchronize_cache10(), None, false)
}

/// BOT: CBW → optional data → CSW (USB MSC spec 5).
fn bot(
    host: &Mutex<Host>,
    slot: u8,
    cdb: &[u8],
    data: Option<&mut [u8]>,
    data_in: bool,
) -> Result<(), Error> {
    let tag = TAG.fetch_add(1, Ordering::Relaxed);
    let data_len = data.as_ref().map(|d| d.len() as u32).unwrap_or(0);
    let cbw = encode_cbw(tag, data_len, data_in && data_len > 0, 0, cdb);
    let mut cbw_buf = cbw;
    // BOT permits only one command until its CSW has completed. Keep the
    // shared controller/bounce-buffer lock across every phase, including data.
    let mut host = host.lock();
    if host.bulk_buf(slot, false, &mut cbw_buf)? != cbw_buf.len() {
        return Err(Error::Protocol);
    }

    if let Some(d) = data {
        if data_in {
            let count = host.bulk_buf(slot, true, d).inspect_err(|e| {
                log::warn!("[k1-usb] msc data-in fail tag={} {:?}", tag, e);
            })?;
            if count != d.len() {
                return Err(Error::Protocol);
            }
        } else {
            if host.bulk_buf(slot, false, d)? != d.len() {
                return Err(Error::Protocol);
            }
        }
    }

    let mut csw_raw = [0u8; CSW_LEN];
    if host.bulk_buf(slot, true, &mut csw_raw)? != CSW_LEN {
        return Err(Error::Protocol);
    }
    let Some(csw) = Csw::parse(&csw_raw) else {
        log::warn!("[k1-usb] msc CSW bad {:02x?}", csw_raw);
        return Err(Error::Protocol);
    };
    if csw.tag != tag {
        log::info!("[k1-usb] msc CSW tag {} != {}", csw.tag, tag);
        return Err(Error::Protocol);
    }
    if !csw.ok() || csw.residue != 0 {
        log::info!(
            "[k1-usb] msc CSW status={} residue={} tag={}",
            csw.status,
            csw.residue,
            tag
        );
        return Err(Error::Protocol);
    }
    Ok(())
}

/// Read/write using the caller's DMA address (bio segment). Bounce only CBW/CSW.
pub(crate) fn xfer_daddr(
    host: &Mutex<Host>,
    slot: u8,
    lba: u32,
    daddr: u64,
    bytes: u32,
    read: bool,
) -> Result<(), Error> {
    if bytes == 0 || !(bytes as usize).is_multiple_of(SECTOR) {
        return Err(Error::Protocol);
    }
    let n = u16::try_from(bytes / SECTOR as u32).map_err(|_| Error::Protocol)?;
    if lba.checked_add(u32::from(n) - 1).is_none() {
        return Err(Error::Protocol);
    }
    let tag = TAG.fetch_add(1, Ordering::Relaxed);
    let cdb = if read {
        cdb_read10(lba, n)
    } else {
        cdb_write10(lba, n)
    };
    let cbw = encode_cbw(tag, bytes, read, 0, &cdb);
    let mut cbw_buf = cbw;
    // BOT permits only one command until its CSW has completed. Keep the
    // shared controller/bounce-buffer lock across every phase, including data.
    let mut host = host.lock();
    if host.bulk_buf(slot, false, &mut cbw_buf)? != cbw_buf.len() {
        return Err(Error::Protocol);
    }
    if host.bulk_daddr(slot, read, daddr, bytes)? != bytes as usize {
        return Err(Error::Protocol);
    }
    let mut csw_raw = [0u8; CSW_LEN];
    if host.bulk_buf(slot, true, &mut csw_raw)? != CSW_LEN {
        return Err(Error::Protocol);
    }
    let Some(csw) = Csw::parse(&csw_raw) else {
        return Err(Error::Protocol);
    };
    if csw.tag != tag || !csw.ok() || csw.residue != 0 {
        log::info!(
            "[k1-usb] msc xfer CSW tag={} status={} want={}",
            csw.tag,
            csw.status,
            tag
        );
        return Err(Error::Protocol);
    }
    Ok(())
}
