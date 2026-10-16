// SPDX-License-Identifier: MPL-2.0

use alloc::vec;

use crate::{
    mm::{
        FrameAllocOptions, PAGE_SIZE, VmReader, VmWriter,
        dma::*,
        io::{VmIo, VmIoOnce, util::HasVmReaderWriter},
    },
    prelude::*,
};

mod dma_coherent {
    use super::*;

    // K1 MUSE Book --- (real DMA allocation/IoMem ownership and cache regressions)
    #[ktest]
    fn dma_iomem_alias_lifetime_regression() {
        use crate::{
            mm::{CachePolicy, PageFlags, PageProperty, Split, VmSpace, vm_space::VmQueriedItem},
            task::disable_preempt,
        };

        for coherent in [false, true] {
            let allocation = DmaCoherent::alloc(2, coherent).unwrap();
            let expected = allocation.cpu_cache_policy();
            let (first, second) = allocation.split(PAGE_SIZE);
            assert_eq!(first.cpu_cache_policy(), expected);
            assert_eq!(second.cpu_cache_policy(), expected);
            drop(second);

            let dma = Arc::new(first);
            let weak = Arc::downgrade(&dma);
            let alias = dma.as_iomem_alias();
            let cloned = alias.clone();
            let sliced = alias.slice(16..32);
            assert_eq!(alias.cache_policy(), expected);
            assert_eq!(sliced.cache_policy(), expected);
            dma.write_val(16, &0x1234_5678u32).unwrap();
            assert_eq!(sliced.read_val::<u32>(0).unwrap(), 0x1234_5678);
            drop(dma);
            drop(alias);
            assert!(weak.upgrade().is_some());
            cloned.write_val(16, &0x9876_5432u32).unwrap();
            assert_eq!(sliced.read_val::<u32>(0).unwrap(), 0x9876_5432);
            drop(cloned);
            assert!(weak.upgrade().is_some());
            drop(sliced);
            assert!(
                weak.upgrade().is_none(),
                "aliases must not form an Arc cycle"
            );
        }

        let dma = Arc::new(DmaCoherent::alloc(1, true).unwrap());
        let weak = Arc::downgrade(&dma);
        let alias = dma.as_iomem_alias();
        let vmspace = VmSpace::new();
        let range = 0x1000..0x2000;
        let preempt_guard = disable_preempt();
        {
            let mut cursor = vmspace.cursor_mut(&preempt_guard, &range).unwrap();
            // Even an incorrect caller request cannot create a DMA cache alias.
            cursor.map_iomem(
                alias,
                PageProperty::new_user(PageFlags::RW, CachePolicy::Uncacheable),
                PAGE_SIZE,
                0,
            );
        }
        {
            let mut cursor = vmspace.cursor(&preempt_guard, &range).unwrap();
            assert!(matches!(
                cursor.query().unwrap().1,
                Some(VmQueriedItem::MappedIoMem { prop, .. })
                    if prop.cache == CachePolicy::Writeback
            ));
        }
        drop(dma);
        assert!(
            weak.upgrade().is_some(),
            "the VmSpace must retain DMA backing"
        );
        {
            let cursor = vmspace.cursor_mut(&preempt_guard, &range).unwrap();
            let (recovered, offset) = cursor
                .find_iomem_by_paddr(weak.upgrade().unwrap().paddr())
                .unwrap();
            assert_eq!(offset, 0);
            assert_eq!(recovered.cache_policy(), CachePolicy::Writeback);
        }
        drop(preempt_guard);
        drop(vmspace);
        assert!(
            weak.upgrade().is_none(),
            "the last VmSpace must release the backing"
        );
    }
    // K1 MUSE Book ---

    #[ktest]
    fn alloc_with_coherent_device() {
        let dma_coherent = DmaCoherent::alloc(1, true).unwrap();
        assert_eq!(dma_coherent.size(), PAGE_SIZE);
    }

    #[ktest]
    fn read_write() {
        let dma_coherent = DmaCoherent::alloc(2, false).unwrap();
        assert_eq!(dma_coherent.size(), 2 * PAGE_SIZE);

        let buf_write = vec![1u8; 2 * PAGE_SIZE];
        dma_coherent.write_bytes(0, &buf_write).unwrap();
        let mut buf_read = vec![0u8; 2 * PAGE_SIZE];
        dma_coherent.read_bytes(0, &mut buf_read).unwrap();
        assert_eq!(buf_write, buf_read);
    }

    #[ktest]
    fn read_write_once() {
        let dma_coherent = DmaCoherent::alloc(2, false).unwrap();

        let buf_write = 1u64;
        dma_coherent.write_once(0, &buf_write).unwrap();
        let buf_read: u64 = dma_coherent.read_once(0).unwrap();
        assert_eq!(buf_read, buf_write);
    }

    #[ktest]
    fn reader_writer() {
        let dma_coherent = DmaCoherent::alloc(2, false).unwrap();

        let buf_write = vec![1u8; PAGE_SIZE];
        let mut writer = dma_coherent.writer();
        writer.write(&mut buf_write.as_slice().into());
        writer.write(&mut buf_write.as_slice().into());

        let mut buf_read = vec![0u8; 2 * PAGE_SIZE];
        let buf_write = vec![1u8; 2 * PAGE_SIZE];
        let mut reader = dma_coherent.reader();
        reader.read(&mut buf_read.as_mut_slice().into());
        assert_eq!(buf_read, buf_write);
    }

    #[ktest]
    fn zero_length_operations() {
        let dma_coherent = DmaCoherent::alloc(1, false).unwrap();

        // Zero-length read/write should succeed
        let empty_buf = [];
        dma_coherent.write_bytes(0, &empty_buf).unwrap();
        let mut empty_buf = [];
        dma_coherent.read_bytes(0, &mut empty_buf).unwrap();
    }

    #[ktest]
    fn complex_read_write_patterns() {
        let dma_coherent = DmaCoherent::alloc(4, false).unwrap();

        // Test alternating pattern
        let pattern1 = vec![0xAAu8; PAGE_SIZE];
        let pattern2 = vec![0x55u8; PAGE_SIZE];
        dma_coherent.write_bytes(0, &pattern1).unwrap();
        dma_coherent.write_bytes(PAGE_SIZE, &pattern2).unwrap();
        dma_coherent.write_bytes(2 * PAGE_SIZE, &pattern1).unwrap();
        dma_coherent.write_bytes(3 * PAGE_SIZE, &pattern2).unwrap();

        let mut read_buf = vec![0u8; 4 * PAGE_SIZE];
        dma_coherent.read_bytes(0, &mut read_buf).unwrap();
        assert_eq!(&read_buf[0..PAGE_SIZE], &pattern1[..]);
        assert_eq!(&read_buf[PAGE_SIZE..2 * PAGE_SIZE], &pattern2[..]);
        assert_eq!(&read_buf[2 * PAGE_SIZE..3 * PAGE_SIZE], &pattern1[..]);
        assert_eq!(&read_buf[3 * PAGE_SIZE..4 * PAGE_SIZE], &pattern2[..]);
    }

    #[ktest]
    fn alloc_uninit_dma_coherent() {
        let dma_coherent = DmaCoherent::alloc_uninit(1, false).unwrap();
        assert_eq!(dma_coherent.size(), PAGE_SIZE);

        let buf_write = vec![0xCDu8; PAGE_SIZE];
        dma_coherent.write_bytes(0, &buf_write).unwrap();

        let mut buf_read = vec![0u8; PAGE_SIZE];
        dma_coherent.read_bytes(0, &mut buf_read).unwrap();
        assert_eq!(buf_write, buf_read);
    }
}

mod dma_stream {
    use super::*;

    // K1 MUSE Book --- (K1 bounce may relocate DMA when the original address is above the limit)
    fn assert_mapped_device_addr(dma_paddr: usize, orig_paddr: usize, orig_size: usize) {
        let limit = crate::mm::dma_addr_limit();
        assert!(
            dma_paddr < limit,
            "mapped DMA address {:#x} is at or above limit {:#x}",
            dma_paddr,
            limit
        );
        if orig_paddr.saturating_add(orig_size) <= limit {
            assert_eq!(dma_paddr, orig_paddr);
        }
    }
    // K1 MUSE Book ---

    #[ktest]
    fn streaming_map() {
        let segment = FrameAllocOptions::new()
            .alloc_segment_with(1, |_| ())
            .unwrap();
        let dma_stream = DmaStream::<FromAndToDevice>::map(segment.clone().into(), true).unwrap();
        // K1 MUSE Book --- (accept bounce when the original address is above the DMA limit)
        // assert_eq!(dma_stream.paddr(), segment.paddr());
        assert_mapped_device_addr(dma_stream.paddr(), segment.paddr(), segment.size());
        // K1 MUSE Book ---
        assert_eq!(dma_stream.size(), PAGE_SIZE);
    }

    #[ktest]
    fn read_write() {
        let segment = FrameAllocOptions::new()
            .alloc_segment_with(2, |_| ())
            .unwrap();
        let dma_stream = DmaStream::<FromAndToDevice>::map(segment.into(), false).unwrap();

        let buf_write = vec![1u8; 2 * PAGE_SIZE];
        dma_stream.write_bytes(0, &buf_write).unwrap();
        dma_stream.sync_to_device(0..2 * PAGE_SIZE).unwrap();
        let mut buf_read = vec![0u8; 2 * PAGE_SIZE];
        dma_stream.sync_from_device(0..2 * PAGE_SIZE).unwrap();
        dma_stream.read_bytes(0, &mut buf_read).unwrap();
        assert_eq!(buf_write, buf_read);
    }

    #[ktest]
    fn reader_writer() {
        let segment = FrameAllocOptions::new()
            .alloc_segment_with(2, |_| ())
            .unwrap();
        let dma_stream = DmaStream::<FromAndToDevice>::map(segment.into(), false).unwrap();

        let buf_write = vec![1u8; PAGE_SIZE];
        let mut writer = dma_stream.writer().unwrap();
        writer.write(&mut buf_write.as_slice().into());
        writer.write(&mut buf_write.as_slice().into());
        dma_stream.sync_to_device(0..2 * PAGE_SIZE).unwrap();
        let mut buf_read = vec![0u8; 2 * PAGE_SIZE];
        let buf_write = vec![1u8; 2 * PAGE_SIZE];
        let mut reader = dma_stream.reader().unwrap();
        dma_stream.sync_from_device(0..2 * PAGE_SIZE).unwrap();
        reader.read(&mut buf_read.as_mut_slice().into());
        assert_eq!(buf_read, buf_write);
    }

    #[ktest]
    fn to_device() {
        let segment = FrameAllocOptions::new()
            .alloc_segment_with(1, |_| ())
            .unwrap();
        let dma_stream = DmaStream::<ToDevice>::map(segment.clone().into(), false).unwrap();
        // K1 MUSE Book --- (accept bounce when the original address is above the DMA limit)
        // assert_eq!(dma_stream.paddr(), segment.paddr());
        assert_mapped_device_addr(dma_stream.paddr(), segment.paddr(), segment.size());
        // K1 MUSE Book ---
        assert_eq!(dma_stream.size(), PAGE_SIZE);

        let mut buffer = [0u8; 8];
        let mut writer_fallible = VmWriter::from(&mut buffer[..]).to_fallible();
        let result = dma_stream.read(0, &mut writer_fallible);
        assert!(result.is_err());

        let buffer = [0u8; 8];
        let mut reader_fallible = VmReader::from(&buffer[..]).to_fallible();
        let result = dma_stream.write(0, &mut reader_fallible);
        assert!(result.is_ok());

        assert!(dma_stream.reader().is_err());
        assert!(dma_stream.writer().is_ok());
    }

    #[ktest]
    fn from_device() {
        let segment = FrameAllocOptions::new()
            .alloc_segment_with(1, |_| ())
            .unwrap();
        let dma_stream = DmaStream::<FromDevice>::map(segment.clone().into(), false).unwrap();
        // K1 MUSE Book --- (accept bounce when the original address is above the DMA limit)
        // assert_eq!(dma_stream.paddr(), segment.paddr());
        assert_mapped_device_addr(dma_stream.paddr(), segment.paddr(), segment.size());
        // K1 MUSE Book ---
        assert_eq!(dma_stream.size(), PAGE_SIZE);

        let mut buffer = [0u8; 8];
        let mut writer_fallible = VmWriter::from(&mut buffer[..]).to_fallible();
        let result = dma_stream.read(0, &mut writer_fallible);
        assert!(result.is_ok());

        let buffer = [0u8; 8];
        let mut reader_fallible = VmReader::from(&buffer[..]).to_fallible();
        let result = dma_stream.write(0, &mut reader_fallible);
        assert!(result.is_err());

        assert!(dma_stream.reader().is_ok());
        assert!(dma_stream.writer().is_err());
    }

    #[ktest]
    fn streaming_boundary_conditions() {
        let segment = FrameAllocOptions::new()
            .alloc_segment_with(2, |_| ())
            .unwrap();
        let dma_stream = DmaStream::<FromAndToDevice>::map(segment.into(), false).unwrap();

        // Test partial page operations
        let small_buf = [0xAAu8; 128];
        dma_stream.write_bytes(PAGE_SIZE - 64, &small_buf).unwrap();
        dma_stream
            .sync_to_device(PAGE_SIZE - 64..PAGE_SIZE + 64)
            .unwrap();
        let mut read_buf = [0u8; 128];
        dma_stream
            .sync_from_device(PAGE_SIZE - 64..PAGE_SIZE + 64)
            .unwrap();
        dma_stream
            .read_bytes(PAGE_SIZE - 64, &mut read_buf)
            .unwrap();
        assert_eq!(read_buf, small_buf);
    }

    #[ktest]
    fn alloc_dma_stream() {
        let dma_stream = DmaStream::<FromAndToDevice>::alloc(2, false).unwrap();
        assert_eq!(dma_stream.size(), 2 * PAGE_SIZE);
        // K1 MUSE Book --- (alloc_below must honour dma_addr_limit)
        assert!(
            dma_stream.paddr() + dma_stream.size() <= crate::mm::dma_addr_limit(),
            "DmaStream::alloc returned {:#x}..{:#x} crossing {:#x}",
            dma_stream.paddr(),
            dma_stream.paddr() + dma_stream.size(),
            crate::mm::dma_addr_limit()
        );
        // K1 MUSE Book ---

        // Verify allocated memory is zeroed
        let mut buf_read = vec![1u8; 2 * PAGE_SIZE];
        dma_stream.sync_from_device(0..2 * PAGE_SIZE).unwrap();
        dma_stream.read_bytes(0, &mut buf_read).unwrap();
        assert_eq!(buf_read, vec![0u8; 2 * PAGE_SIZE]);

        let buf_write = vec![0xABu8; 2 * PAGE_SIZE];
        dma_stream.write_bytes(0, &buf_write).unwrap();
        dma_stream.sync_to_device(0..2 * PAGE_SIZE).unwrap();

        let mut buf_read = vec![0u8; 2 * PAGE_SIZE];
        dma_stream.sync_from_device(0..2 * PAGE_SIZE).unwrap();
        dma_stream.read_bytes(0, &mut buf_read).unwrap();
        assert_eq!(buf_write, buf_read);
    }

    #[ktest]
    fn alloc_uninit_dma_stream() {
        let dma_stream = DmaStream::<FromAndToDevice>::alloc_uninit(1, false).unwrap();
        assert_eq!(dma_stream.size(), PAGE_SIZE);

        let buf_write = vec![0xCDu8; PAGE_SIZE];
        dma_stream.write_bytes(0, &buf_write).unwrap();
        dma_stream.sync_to_device(0..PAGE_SIZE).unwrap();

        let mut buf_read = vec![0u8; PAGE_SIZE];
        dma_stream.sync_from_device(0..PAGE_SIZE).unwrap();
        dma_stream.read_bytes(0, &mut buf_read).unwrap();
        assert_eq!(buf_write, buf_read);
    }

    #[ktest]
    fn sync_invalid_ranges() {
        let dma_stream = DmaStream::<FromAndToDevice>::alloc(2, false).unwrap();
        assert_eq!(dma_stream.size(), 2 * PAGE_SIZE);

        #[expect(clippy::reversed_empty_ranges)]
        const INVALID_RANGES: [core::ops::Range<usize>; 5] = [
            // Reversed ranges:
            PAGE_SIZE..0,
            2 * PAGE_SIZE..PAGE_SIZE,
            // Out-of-bounds ranges:
            PAGE_SIZE..4 * PAGE_SIZE,
            2 * PAGE_SIZE..4 * PAGE_SIZE,
            3 * PAGE_SIZE..4 * PAGE_SIZE,
        ];

        for invalid_range in INVALID_RANGES {
            assert_eq!(
                dma_stream.sync_from_device(invalid_range.clone()),
                Err(crate::Error::InvalidArgs)
            );
            assert_eq!(
                dma_stream.sync_to_device(invalid_range),
                Err(crate::Error::InvalidArgs)
            );
        }
    }
}

// K1 MUSE Book --- (both synchronization endpoints are part of the safety bound)
#[ktest]
fn dma_stream_sync_range_bounds_regression() {
    let dma = DmaStream::<FromAndToDevice>::alloc(1, true).unwrap();
    for range in [
        core::ops::Range { start: 2, end: 1 },
        core::ops::Range {
            start: 0,
            end: PAGE_SIZE + 1,
        },
        core::ops::Range {
            start: PAGE_SIZE + 1,
            end: PAGE_SIZE + 1,
        },
    ] {
        assert!(matches!(
            dma.sync_to_device(range.clone()),
            Err(error::Error::InvalidArgs)
        ));
        assert!(matches!(
            dma.sync_from_device(range),
            Err(error::Error::InvalidArgs)
        ));
    }
    for range in [0..0, PAGE_SIZE..PAGE_SIZE, 0..PAGE_SIZE, 1..PAGE_SIZE] {
        assert!(dma.sync_to_device(range.clone()).is_ok());
        assert!(dma.sync_from_device(range).is_ok());
    }
}
// K1 MUSE Book ---
