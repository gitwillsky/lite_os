//! Memory-owned frame adapter for the architecture-owned page-table mechanism.

use crate::{
    arch::mmu::{ArchitecturePageTable, ArchitecturePageTableEntry, TablePage},
    memory::{
        address::{PhysicalPageNumber, VirtualPageNumber},
        frame_allocator::{self, FrameTracker},
        mm::shootdown::{TranslationCommit, TranslationTransition},
    },
};

pub(crate) use crate::arch::mmu::{PagePermissions, PageTableError};

impl TablePage for FrameTracker {
    fn allocate() -> Option<Self> {
        frame_allocator::alloc()
    }

    fn physical_page(&self) -> usize {
        self.ppn.as_usize()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PageTableEntry(ArchitecturePageTableEntry);

impl PageTableEntry {
    pub(crate) fn permissions(self) -> PagePermissions {
        self.0.permissions()
    }

    pub(crate) fn ppn(self) -> PhysicalPageNumber {
        self.0.physical_page().into()
    }
}

pub(crate) struct PageTable(ArchitecturePageTable<FrameTracker>);

impl core::fmt::Debug for PageTable {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("PageTable")
    }
}

impl PageTable {
    pub(crate) fn new() -> Self {
        Self::try_new().expect("cannot allocate root frame for page table")
    }

    pub(crate) fn try_new() -> Result<Self, PageTableError> {
        ArchitecturePageTable::try_new().map(Self)
    }

    pub(crate) fn token(&self) -> crate::arch::mmu::AddressSpaceToken {
        self.0.token()
    }

    /// @description 在全 CPU fence 完成后把 architecture ASID 交还唯一 allocator。
    pub(crate) fn release_address_space_id_after_global_fence(&mut self) {
        self.0.release_address_space_id_after_global_fence();
    }

    pub(crate) fn reserve(&mut self, vpn: VirtualPageNumber) -> Result<(), PageTableError> {
        self.0.reserve(usize::from(vpn))
    }

    pub(in crate::memory) fn map(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        self.0
            .map(usize::from(vpn), usize::from(ppn), permissions)?;
        commit.record(vpn.as_usize(), TranslationTransition::Publish);
        if permissions.contains(PagePermissions::EXECUTE) {
            commit.record_instruction_publication();
        }
        Ok(())
    }

    pub(in crate::memory) fn map_identity_range(
        &mut self,
        start: VirtualPageNumber,
        end: VirtualPageNumber,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        let start = start.as_usize();
        let end = end.as_usize();
        self.0.map_identity_range(start, end, permissions)?;
        commit.record_range(start, end - start, TranslationTransition::Publish);
        if permissions.contains(PagePermissions::EXECUTE) {
            commit.record_instruction_publication();
        }
        Ok(())
    }

    pub(in crate::memory) fn unmap(
        &mut self,
        vpn: VirtualPageNumber,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        // active AVL node 与 frame owner 一起无分配移交给 fence commit；rollback/OOM 路径
        // 不得在 PTE 已撤销后再次申请 retention storage。
        let unmapped = self.0.unmap(usize::from(vpn))?;
        let (first_page, page_count, retired) = unmapped.into_parts();
        commit.retain_table_pages(retired);
        commit.record_range(first_page, page_count, TranslationTransition::Revoke);
        Ok(())
    }

    pub(in crate::memory) fn set_flags(
        &mut self,
        vpn: VirtualPageNumber,
        permissions: PagePermissions,
        commit: &mut TranslationCommit,
    ) -> Result<(), PageTableError> {
        let old = self
            .translate(vpn)
            .ok_or(PageTableError::NotMapped)?
            .permissions();
        self.0.set_flags(usize::from(vpn), permissions)?;
        if permissions != old {
            commit.record(
                vpn.as_usize(),
                if permissions.contains(old) {
                    TranslationTransition::Relax
                } else {
                    TranslationTransition::Revoke
                },
            );
            if permissions.contains(PagePermissions::EXECUTE)
                && !old.contains(PagePermissions::EXECUTE)
            {
                commit.record_instruction_publication();
            }
        }
        Ok(())
    }

    pub(crate) fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.0.translate(usize::from(vpn)).map(PageTableEntry)
    }
}
