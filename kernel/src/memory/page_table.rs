//! Memory-owned frame adapter for the architecture-owned page-table mechanism.

use crate::{
    arch::mmu::{ArchitecturePageTable, ArchitecturePageTableEntry, TablePage},
    memory::{
        address::{PhysicalPageNumber, VirtualPageNumber},
        frame_allocator::{self, FrameTracker},
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

    pub(crate) fn reserve(&mut self, vpn: VirtualPageNumber) -> Result<(), PageTableError> {
        self.0.reserve(usize::from(vpn))
    }

    pub(crate) fn map(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        permissions: PagePermissions,
    ) -> Result<(), PageTableError> {
        self.0.map(usize::from(vpn), usize::from(ppn), permissions)
    }

    pub(crate) fn unmap(&mut self, vpn: VirtualPageNumber) -> Result<(), PageTableError> {
        self.0.unmap(usize::from(vpn))
    }

    pub(crate) fn set_flags(
        &mut self,
        vpn: VirtualPageNumber,
        permissions: PagePermissions,
    ) -> Result<(), PageTableError> {
        self.0.set_flags(usize::from(vpn), permissions)
    }

    pub(crate) fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.0.translate(usize::from(vpn)).map(PageTableEntry)
    }
}
