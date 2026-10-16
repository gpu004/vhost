use super::*;
use hegel::generators as gs;
use hegel::TestCase;
use std::collections::BTreeSet;
use std::io::Read;
use std::os::fd::AsFd;
use vm_memory::{GuestAddress, GuestRegionMmap};

const PAGE_SIZE: usize = 4096;

fn verify_dirty_log(tc: TestCase, page_aligned: bool) {
    let start_page = tc.draw(gs::integers::<usize>().min_value(0).max_value(64));
    let start_offset = if page_aligned || tc.draw(gs::booleans()) {
        0
    } else {
        tc.draw(
            gs::integers::<usize>()
                .min_value(1)
                .max_value(PAGE_SIZE - 1),
        )
    };
    let region_start = start_page * PAGE_SIZE + start_offset;
    let region_len = if page_aligned || tc.draw(gs::booleans()) {
        tc.draw(gs::integers::<usize>().min_value(1).max_value(64)) * PAGE_SIZE
    } else {
        tc.draw(
            gs::integers::<usize>()
                .min_value(1)
                .max_value(64 * PAGE_SIZE),
        )
    };
    let region =
        GuestRegionMmap::<()>::from_range(GuestAddress(region_start as u64), region_len, None)
            .unwrap();
    let page_count = (region_start + region_len).div_ceil(PAGE_SIZE);
    let log_len = page_count.div_ceil(u8::BITS as usize) + 1;
    let file = tempfile::tempfile().unwrap();
    file.set_len(log_len as u64).unwrap();
    let logmem = Arc::new(MmapLogReg::from_file(file.as_fd(), 0, log_len as u64).unwrap());
    let bitmap = BitmapMmapRegion::default();
    bitmap.replace(AtomicBitmapMmap::new(&region, logmem).unwrap());

    let write_count = tc.draw(gs::integers::<usize>().min_value(1).max_value(8));
    let mut writes = Vec::with_capacity(write_count);
    let mut touched_pages = BTreeSet::new();
    for _ in 0..write_count {
        let offset = tc.draw(
            gs::integers::<usize>()
                .min_value(0)
                .max_value(region_len - 1),
        );
        let len = tc.draw(
            gs::integers::<usize>()
                .min_value(1)
                .max_value(region_len - offset),
        );
        writes.push((offset, len));
        let first_page = (region_start + offset) / PAGE_SIZE;
        let last_page = (region_start + offset + len - 1) / PAGE_SIZE;
        touched_pages.extend(first_page..=last_page);
        bitmap.mark_dirty(offset, len);
    }

    for (offset, len) in writes {
        for written_offset in offset..offset + len {
            assert!(
                bitmap.dirty_at(written_offset),
                "written byte offset {written_offset} is not dirty"
            );
        }
    }

    let mut log_bytes = vec![0; log_len];
    (&file).read_exact(&mut log_bytes).unwrap();
    for page in 0..log_len * u8::BITS as usize {
        let is_dirty = log_bytes[page / u8::BITS as usize] & (1 << (page % u8::BITS as usize)) != 0;
        assert_eq!(
            is_dirty,
            touched_pages.contains(&page),
            "unexpected log state for absolute page {page}"
        );
    }
}

#[hegel::test]
#[hegel::reproduce_failure("AXicY2cAAi5GEMnIyAhlMqJQMEEABk4ARQ==")]
fn hegel_bitmap_dirty_log_unaligned(tc: TestCase) {
    verify_dirty_log(tc, false);
}

#[hegel::test]
fn hegel_bitmap_dirty_log_page_aligned(tc: TestCase) {
    verify_dirty_log(tc, true);
}
