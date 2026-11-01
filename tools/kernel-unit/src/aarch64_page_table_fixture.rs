#![allow(dead_code)]

mod mmu {
    #[derive(Clone, Copy)]
    pub(crate) struct AddressSpaceToken;
    #[derive(Clone, Copy)]
    pub(crate) struct KernelTrapToken;

    impl AddressSpaceToken {
        pub(crate) fn from_root_page(
            _root_page: usize,
            _context: std::sync::Arc<AddressSpaceContext>,
        ) -> Self {
            Self
        }
    }

    pub(crate) struct AddressSpaceContext;
    impl AddressSpaceContext {
        pub(crate) fn try_new() -> Result<std::sync::Arc<Self>, ()> {
            Ok(std::sync::Arc::new(Self))
        }
        pub(crate) fn retire_after_global_fence(&self) {}
    }
    pub(crate) fn activate_kernel(_root_page: usize) {}
    // OWNER: PAGE_TABLE_TEST 串行化实际页表夹具；每个稀疏扫描在该锁内清零并读取访问计数。
    pub(crate) static TABLE_ACCESSES: core::sync::atomic::AtomicUsize =
        core::sync::atomic::AtomicUsize::new(0);
    pub(crate) fn physical_to_virtual(address: usize) -> usize {
        TABLE_ACCESSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        address
    }
}

#[path = "../../../kernel/src/arch/aarch64/page_table.rs"]
mod page_table;
#[path = "../../../kernel/src/arch/aarch64/pte.rs"]
mod pte;
#[path = "../../../kernel/src/arch/aarch64/va39.rs"]
mod va39;

#[cfg(test)]
mod tests {
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::{
        page_table::{AddressSpaceKind, PageTable, TablePage},
        pte::PagePermissions,
    };

    static LIVE_TABLE_PAGES: AtomicUsize = AtomicUsize::new(0);
    static PAGE_TABLE_TEST: Mutex<()> = Mutex::new(());

    struct HostTablePage(*mut u8);

    impl TablePage for HostTablePage {
        fn allocate() -> Option<Self> {
            let layout = Layout::from_size_align(4096, 4096).unwrap();
            // SAFETY: fixed nonzero layout；null is handled as allocation failure。
            let pointer = unsafe { alloc_zeroed(layout) };
            if pointer.is_null() {
                return None;
            }
            LIVE_TABLE_PAGES.fetch_add(1, Ordering::Relaxed);
            Some(Self(pointer))
        }

        fn physical_page(&self) -> usize {
            self.0 as usize >> 12
        }
    }

    impl Drop for HostTablePage {
        fn drop(&mut self) {
            let layout = Layout::from_size_align(4096, 4096).unwrap();
            // SAFETY: pointer came from alloc_zeroed with the identical live layout。
            unsafe { dealloc(self.0, layout) };
            LIVE_TABLE_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn isolated_leaf_detaches_empty_tables_but_retains_frames_for_fence() {
        let _serial = PAGE_TABLE_TEST.lock().unwrap();
        assert_eq!(LIVE_TABLE_PAGES.load(Ordering::Relaxed), 0);
        let mut table = PageTable::<HostTablePage>::try_new(AddressSpaceKind::User).unwrap();
        table
            .map(
                0x4_0201,
                0xabc,
                PagePermissions::READ | PagePermissions::USER,
            )
            .unwrap();
        assert_eq!(table.table_page_count(), 3);
        assert_eq!(LIVE_TABLE_PAGES.load(Ordering::Relaxed), 3);

        let unmapped = table.unmap(0x4_0201).unwrap();
        let (first_page, page_count, retired) = unmapped.into_parts();
        assert_eq!((first_page, page_count), (0x4_0201, 1));
        assert_eq!(retired.len(), 2);
        assert_eq!(table.table_page_count(), 1);
        assert_eq!(LIVE_TABLE_PAGES.load(Ordering::Relaxed), 3);
        drop(retired);
        assert_eq!(LIVE_TABLE_PAGES.load(Ordering::Relaxed), 1);
        drop(table);
        assert_eq!(LIVE_TABLE_PAGES.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn aligned_identity_range_uses_middle_leaves_without_crossing_permissions() {
        let _serial = PAGE_TABLE_TEST.lock().unwrap();
        let mut table = PageTable::<HostTablePage>::try_new(AddressSpaceKind::User).unwrap();
        let start = 0x8_0000;
        let middle_pages = 512;
        table
            .map_contiguous_range(
                start,
                start,
                middle_pages,
                PagePermissions::READ | PagePermissions::USER,
            )
            .unwrap();
        table
            .map_contiguous_range(
                start + middle_pages,
                start + middle_pages,
                middle_pages,
                PagePermissions::READ | PagePermissions::WRITE,
            )
            .unwrap();
        assert_eq!(table.table_page_count(), 2);
        let first = table.translate(start + 37).unwrap();
        assert_eq!(first.physical_page(), start + 37);
        assert_eq!(
            first.permissions(),
            PagePermissions::READ | PagePermissions::USER
        );
        let second = table.translate(start + middle_pages + 19).unwrap();
        assert_eq!(second.physical_page(), start + middle_pages + 19);
        assert_eq!(
            second.permissions(),
            PagePermissions::READ | PagePermissions::WRITE
        );
    }

    #[test]
    fn middle_leaf_revoke_requires_base_and_reports_full_fence_span() {
        let _serial = PAGE_TABLE_TEST.lock().unwrap();
        let mut table = PageTable::<HostTablePage>::try_new(AddressSpaceKind::User).unwrap();
        let start = 0x10_0000;
        table
            .map_contiguous_range(start, start, 512, PagePermissions::READ)
            .unwrap();
        assert!(matches!(
            table.unmap(start + 1),
            Err(super::page_table::PageTableError::NotMapped)
        ));
        assert!(table.translate(start + 1).is_some());
        let (first_page, page_count, retired) = table.unmap(start).unwrap().into_parts();
        assert_eq!((first_page, page_count), (start, 512));
        assert_eq!(retired.len(), 1);
        assert_eq!(table.table_page_count(), 1);
    }
    #[test]
    fn sparse_range_lookup_skips_absent_branches_and_survives_retirement() {
        let _serial = PAGE_TABLE_TEST.lock().unwrap();
        let mut table = PageTable::<HostTablePage>::try_new(AddressSpaceKind::User).unwrap();
        let far = (1usize << 25) - 1;
        let permissions = PagePermissions::READ | PagePermissions::USER;
        table.map(far, 1234, permissions).unwrap();
        super::mmu::TABLE_ACCESSES.store(0, Ordering::Relaxed);
        let (page, entry) = table.next_mapped(0, far + 1).unwrap();
        assert_eq!(
            (page, entry.physical_page(), entry.permissions()),
            (far, 1234, permissions)
        );
        assert!(
            super::mmu::TABLE_ACCESSES.load(Ordering::Relaxed) < 3 * 512 * table.table_page_count(),
            "sparse traversal scanned 4KiB holes instead of table branches"
        );
        assert!(table.next_mapped(0, far).is_none());
        let (_, _, retired) = table.unmap(far).unwrap().into_parts();
        assert!(table.next_mapped(0, far + 1).is_none());
        drop(retired);
    }

    #[test]
    fn sparse_lookup_inside_a_large_leaf_preserves_offset_and_end_bound() {
        let _serial = PAGE_TABLE_TEST.lock().unwrap();
        let mut table = PageTable::<HostTablePage>::try_new(AddressSpaceKind::User).unwrap();
        let first = 1 << 18;
        table
            .map_contiguous_range(first, first * 2, 512, PagePermissions::READ)
            .unwrap();
        let (page, entry) = table.next_mapped(first + 37, first + 38).unwrap();
        assert_eq!((page, entry.physical_page()), (first + 37, first * 2 + 37));
        assert!(table.next_mapped(first, first).is_none());
    }
}
