// SPDX-License-Identifier: MPL-2.0

// K1 MUSE Book: SCSI CDB helpers for USB MSC.

//! SCSI / Bulk-Only Transport (BOT) CDBs and wrappers. Pure encode.

pub(crate) const CBW_SIG: u32 = 0x4342_5355; // "USBC"
pub(crate) const CSW_SIG: u32 = 0x5342_5355; // "USBS"
pub(crate) const CBW_LEN: usize = 31;
pub(crate) const CSW_LEN: usize = 13;
pub(crate) const CBW_FLAG_IN: u8 = 0x80;

pub(crate) const SCSI_TEST_UNIT_READY: u8 = 0x00;
pub(crate) const SCSI_REQUEST_SENSE: u8 = 0x03;
pub(crate) const SCSI_INQUIRY: u8 = 0x12;
pub(crate) const SCSI_READ_CAPACITY_10: u8 = 0x25;
pub(crate) const SCSI_READ_10: u8 = 0x28;
pub(crate) const SCSI_WRITE_10: u8 = 0x2a;
pub(crate) const SCSI_SYNCHRONIZE_CACHE_10: u8 = 0x35;

pub(crate) fn cdb_inquiry(alloc: u8) -> [u8; 6] {
    let mut c = [0u8; 6];
    c[0] = SCSI_INQUIRY;
    c[4] = alloc;
    c
}

pub(crate) fn cdb_test_unit_ready() -> [u8; 6] {
    [SCSI_TEST_UNIT_READY, 0, 0, 0, 0, 0]
}

pub(crate) fn cdb_request_sense(alloc: u8) -> [u8; 6] {
    let mut c = [0u8; 6];
    c[0] = SCSI_REQUEST_SENSE;
    c[4] = alloc;
    c
}

pub(crate) fn cdb_read_capacity10() -> [u8; 10] {
    let mut c = [0u8; 10];
    c[0] = SCSI_READ_CAPACITY_10;
    c
}

pub(crate) fn cdb_read10(lba: u32, blocks: u16) -> [u8; 10] {
    let mut c = [0u8; 10];
    c[0] = SCSI_READ_10;
    c[2] = (lba >> 24) as u8;
    c[3] = (lba >> 16) as u8;
    c[4] = (lba >> 8) as u8;
    c[5] = lba as u8;
    c[7] = (blocks >> 8) as u8;
    c[8] = blocks as u8;
    c
}

pub(crate) fn cdb_write10(lba: u32, blocks: u16) -> [u8; 10] {
    let mut c = cdb_read10(lba, blocks);
    c[0] = SCSI_WRITE_10;
    c
}

/// SBC SYNCHRONIZE CACHE(10): IMMED=0, LBA=0, block count=0 (all blocks).
pub(crate) fn cdb_synchronize_cache10() -> [u8; 10] {
    let mut c = [0u8; 10];
    c[0] = SCSI_SYNCHRONIZE_CACHE_10;
    c
}

/// Encode a 31-byte Command Block Wrapper (USB MSC BOT 5.1).
pub(crate) fn encode_cbw(
    tag: u32,
    data_len: u32,
    dir_in: bool,
    lun: u8,
    cdb: &[u8],
) -> [u8; CBW_LEN] {
    let mut b = [0u8; CBW_LEN];
    b[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&data_len.to_le_bytes());
    b[12] = if dir_in { CBW_FLAG_IN } else { 0 };
    b[13] = lun & 0x0f;
    let n = cdb.len().min(16);
    b[14] = n as u8;
    b[15..15 + n].copy_from_slice(&cdb[..n]);
    b
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Csw {
    pub tag: u32,
    pub residue: u32,
    pub status: u8,
}

impl Csw {
    pub(crate) fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < CSW_LEN {
            return None;
        }
        let sig = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if sig != CSW_SIG {
            return None;
        }
        Some(Self {
            tag: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            residue: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            status: b[12],
        })
    }

    pub(crate) fn ok(self) -> bool {
        self.status == 0
    }
}

/// READ CAPACITY(10) data: last LBA + block length (big-endian).
pub(crate) fn parse_capacity10(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 8 {
        return None;
    }
    let last = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let blen = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
    Some((last, blen))
}

#[cfg(ktest)]
mod test {
    use ostd::prelude::ktest;

    use super::*;

    #[ktest]
    fn cdb_read10_lba_and_len() {
        let c = cdb_read10(0x0001_0203, 8);
        assert_eq!(c[0], SCSI_READ_10);
        assert_eq!(&c[2..6], &[0x00, 0x01, 0x02, 0x03]);
        assert_eq!(&c[7..9], &[0x00, 0x08]);
        let w = cdb_write10(1, 1);
        assert_eq!(w[0], SCSI_WRITE_10);
        assert_eq!(&w[2..6], &[0, 0, 0, 1]);
    }

    #[ktest]
    fn cbw_signature_and_flags() {
        let cdb = cdb_inquiry(36);
        let b = encode_cbw(7, 36, true, 0, &cdb[..6]);
        assert_eq!(&b[0..4], b"USBC");
        assert_eq!(u32::from_le_bytes([b[4], b[5], b[6], b[7]]), 7);
        assert_eq!(u32::from_le_bytes([b[8], b[9], b[10], b[11]]), 36);
        assert_eq!(b[12], CBW_FLAG_IN);
        assert_eq!(b[14], 6);
        assert_eq!(b[15], SCSI_INQUIRY);
        assert_eq!(b[19], 36);
    }

    #[ktest]
    fn csw_parse_ok_and_bad_sig() {
        let mut raw = [0u8; 13];
        raw[0..4].copy_from_slice(&CSW_SIG.to_le_bytes());
        raw[4..8].copy_from_slice(&7u32.to_le_bytes());
        assert!(Csw::parse(&raw).unwrap().ok());
        raw[0] = b'X';
        assert!(Csw::parse(&raw).is_none());
        assert!(Csw::parse(&[0u8; 8]).is_none());
    }

    #[ktest]
    fn synchronize_cache_waits_for_all_blocks_without_data() {
        let c = cdb_synchronize_cache10();
        assert_eq!(c, [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let cbw = encode_cbw(19, 0, false, 0, &c);
        assert_eq!(&cbw[8..12], &[0; 4]);
        assert_eq!(cbw[12], 0);
        assert_eq!(cbw[14], 10);
        assert_eq!(&cbw[15..25], &c);
    }

    #[ktest]
    fn capacity10_be() {
        let b = [0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00];
        let (last, blen) = parse_capacity10(&b).unwrap();
        assert_eq!(last, 0x0001_0000);
        assert_eq!(blen, 512);
    }
}
