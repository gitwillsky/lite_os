#![allow(dead_code)]

mod mmu {
    #[derive(Clone, Copy)]
    pub(crate) struct AddressSpaceToken;

    impl AddressSpaceToken {
        pub(crate) fn from_root_page(_root_page: usize, _address_space_id: usize) -> Self {
            Self
        }
    }

    pub(crate) fn allocate_address_space_id() -> Option<usize> {
        Some(1)
    }

    pub(crate) fn release_address_space_id_after_global_fence(_address_space_id: usize) {}
}

#[path = "../../../kernel/src/arch/riscv64/page_table.rs"]
mod page_table;
#[path = "../../../kernel/src/arch/riscv64/pte.rs"]
mod pte;
#[path = "../../../kernel/src/arch/riscv64/sv39.rs"]
mod sv39;

#[cfg(test)]
mod tests {
    use std::alloc::{Layout, alloc_zeroed, dealloc};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::{
        page_table::{PageTable, TablePage},
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
        let mut table = PageTable::<HostTablePage>::try_new().unwrap();
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
        let mut table = PageTable::<HostTablePage>::try_new().unwrap();
        let start = 0x8_0000;
        let middle_pages = 512;
        table
            .map_identity_range(
                start,
                start + middle_pages,
                PagePermissions::READ | PagePermissions::USER,
            )
            .unwrap();
        table
            .map_identity_range(
                start + middle_pages,
                start + 2 * middle_pages,
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
        let mut table = PageTable::<HostTablePage>::try_new().unwrap();
        let start = 0x10_0000;
        table
            .map_identity_range(start, start + 512, PagePermissions::READ)
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
}
