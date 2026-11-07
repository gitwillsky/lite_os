//! Production execution views and both page-table owners; host frames/TLB/trampoline are explicit seams.
extern crate alloc;

#[expect(
    dead_code,
    reason = "host frame adapter is shared in shape with the VMO fixture; views only allocate table frames"
)]
mod memory {
    std::thread_local! {
        pub(crate) static FAIL_AFTER: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
        pub(crate) static LIVE: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }
    #[derive(Debug, Clone, Copy)]
    pub(crate) struct VirtualPageNumber(usize);
    impl VirtualPageNumber {
        pub(crate) fn from_vpn(value: usize) -> Self {
            Self(value)
        }
        pub(crate) fn as_usize(self) -> usize {
            self.0
        }
    }
    impl From<VirtualPageNumber> for usize {
        fn from(value: VirtualPageNumber) -> Self {
            value.0
        }
    }
    impl From<PhysicalPageNumber> for usize {
        fn from(value: PhysicalPageNumber) -> Self {
            value.0
        }
    }

    pub(crate) const PAGE_SIZE: usize = 4096;
    pub(crate) enum FrameAllocationClass {
        Reclaimable,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct PhysicalPageNumber(usize);
    impl From<usize> for PhysicalPageNumber {
        fn from(value: usize) -> Self {
            Self(value)
        }
    }
    impl PhysicalPageNumber {
        pub(crate) fn as_usize(self) -> usize {
            self.0
        }
        pub(crate) fn as_page_ptr(self) -> *const u8 {
            (self.0 * PAGE_SIZE) as *const u8
        }
        pub(crate) fn as_page_mut_ptr(self) -> *mut u8 {
            (self.0 * PAGE_SIZE) as *mut u8
        }
    }

    #[derive(Debug)]
    pub(crate) struct FrameTracker {
        pub(crate) ppn: PhysicalPageNumber,
        layout: std::alloc::Layout,
    }
    impl FrameTracker {
        pub(crate) fn bytes(&self) -> &[u8] {
            // SAFETY: tracker 独占本 layout 的宿主分配，借用不越过 tracker 生命周期。
            unsafe { core::slice::from_raw_parts(self.ppn.as_page_ptr(), self.layout.size()) }
        }
    }
    impl Drop for FrameTracker {
        fn drop(&mut self) {
            LIVE.with(|live| live.set(live.get() - 1));
            // SAFETY: 分配与释放使用同一 layout，tracker 是唯一 owner。
            unsafe { std::alloc::dealloc(self.ppn.as_page_mut_ptr(), self.layout) };
        }
    }
    pub(crate) fn alloc_contiguous(pages: usize, _: FrameAllocationClass) -> Option<FrameTracker> {
        let fail = FAIL_AFTER.with(|remaining| match remaining.get() {
            None => false,
            Some(0) => true,
            Some(count) => {
                remaining.set(Some(count - 1));
                false
            }
        });
        if fail {
            return None;
        }
        let layout =
            std::alloc::Layout::from_size_align(pages.checked_mul(PAGE_SIZE)?, PAGE_SIZE).ok()?;
        // SAFETY: 有效非零 layout，由返回的 tracker 唯一拥有。
        let pointer = core::ptr::NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) })?;
        LIVE.with(|live| live.set(live.get() + 1));
        Some(FrameTracker {
            ppn: PhysicalPageNumber(pointer.as_ptr() as usize / PAGE_SIZE),
            layout,
        })
    }
    pub(crate) mod frame_allocator {
        use super::*;
        pub(crate) fn alloc() -> Option<FrameTracker> {
            alloc_contiguous(1, FrameAllocationClass::Reclaimable)
        }
        pub(crate) fn alloc_copy(bytes: &[u8]) -> Option<FrameTracker> {
            let frame = alloc()?;
            // SAFETY: 新页独占、足够大，与输入页不重叠。
            unsafe {
                core::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    frame.ppn.as_page_mut_ptr(),
                    bytes.len(),
                )
            };
            Some(frame)
        }
    }
}

#[path = "../../../kernel/src/arch/aarch64"]
mod arch {
    #[expect(dead_code, reason = "view fixture only exercises user roots")]
    pub(crate) mod page_table;
    pub(crate) mod pte;
    mod va39;
    pub(crate) mod mmu {
        pub(crate) use super::page_table::PageTableError;
        pub(crate) use super::page_table::{
            AddressSpaceKind, PageTable as ArchitecturePageTable,
            PageTableEntry as ArchitecturePageTableEntry, TablePage,
        };
        pub(crate) use super::pte::PagePermissions;
        pub(crate) const TRAMPOLINE_ADDRESS: usize = (1 << 38) - 4096;
        #[derive(Clone)]
        pub(crate) struct AddressSpaceToken(pub(crate) usize);
        pub(crate) struct KernelTrapToken;
        impl AddressSpaceToken {
            pub(crate) fn from_root_page(
                page: usize,
                _: alloc::sync::Arc<AddressSpaceContext>,
            ) -> Self {
                Self(page)
            }
        }
        pub(crate) struct AddressSpaceContext;
        impl AddressSpaceContext {
            pub(crate) fn try_new() -> Result<alloc::sync::Arc<Self>, ()> {
                Ok(alloc::sync::Arc::new(Self))
            }
            pub(crate) fn retire_after_global_fence(&self) {}
        }
        pub(crate) fn activate_kernel(_: usize) {
            unreachable!("host view fixture does not activate MMU")
        }
        pub(crate) fn physical_to_virtual(address: usize) -> usize {
            address
        }
    }
}
#[path = "../../../kernel/src/vm"]
mod vm {
    #[derive(Debug, PartialEq)]
    pub(crate) enum VmError {
        NoMemory,
        BadState,
    }
    pub(crate) const USER_END: usize = (1 << 38) - 8192;
    pub(crate) mod address_space {
        pub(crate) const SHARED_START: usize = 1 << 37;
        pub(crate) enum ZapMode {
            Unmap,
            RemoveWrite,
        }
    }
    // The host does not execute a trampoline; target guest gates verify its real mapping.
    fn map_trampoline(
        _: &mut page_table::PageTable,
        _: &mut translation::TranslationCommit,
    ) -> Result<(), VmError> {
        Ok(())
    }
    #[expect(
        dead_code,
        reason = "actual VM frame adapter also supports kernel-only mappings"
    )]
    pub(crate) mod page_table;
    #[expect(
        dead_code,
        reason = "target fence/IPI adapters are replaced; record/retirement ownership remains production code"
    )]
    pub(crate) mod translation;
    impl translation::TranslationCommit {
        pub(super) fn synchronize(&mut self) -> Result<(), &'static str> {
            core::mem::replace(self, Self::new()).finish_unpublished();
            Ok(())
        }
    }
    #[expect(
        dead_code,
        reason = "guest handoff selects hardware tokens; host checks actual leaf visibility and retirement"
    )]
    mod execution_views;
    use execution_views::ExecutionViews;

    #[test]
    fn partial_context_publication_oom_is_revoked_before_frame_release() {
        use crate::memory::{self, VirtualPageNumber, frame_allocator};
        let before = memory::LIVE.with(core::cell::Cell::get);
        {
            let mut views = ExecutionViews::new(true).unwrap();
            assert_ne!(views.normal.token().0, views.private_token().unwrap().0);
            let frame = frame_allocator::alloc().unwrap();
            let vpn = VirtualPageNumber::from_vpn(0x3fff_fff0);
            // The normal root can allocate its two intermediate levels; private allocation then fails.
            memory::FAIL_AFTER.with(|fail| fail.set(Some(2)));
            let mut commit = translation::TranslationCommit::new();
            assert_eq!(
                views.map_context(vpn, frame.ppn, &mut commit),
                Err(VmError::NoMemory)
            );
            views.unmap_context(vpn, &mut commit);
            commit.synchronize().unwrap();
            memory::FAIL_AFTER.with(|fail| fail.set(None));
            views.map_context(vpn, frame.ppn, &mut commit).unwrap();
            commit.synchronize().unwrap();
            views.unmap_context(vpn, &mut commit);
            commit.synchronize().unwrap();
            views.retire();
        }
        assert_eq!(memory::LIVE.with(core::cell::Cell::get), before);
    }
}
