use alloc::vec::Vec;

use super::{
    mmu::AddressSpaceToken,
    pte::{self, PagePermissions, RiscvPteFlags},
    sv39,
};

const PTE_FLAGS_WIDTH: usize = 10;
const PAGE_SHIFT: usize = 12;

/// @description Architecture page-table page allocation seam。
///
/// Implementor owns physical-frame policy and lifetime; the Sv39 walker owns only table layout。
pub(crate) trait TablePage: Sized {
    fn allocate() -> Option<Self>;
    fn physical_page(&self) -> usize;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageTableError {
    AlreadyMapped,
    NotMapped,
    OutOfMemory,
    InvalidFlags,
    InvalidPageTable,
}

impl core::fmt::Display for PageTableError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyMapped => write!(formatter, "page is already mapped"),
            Self::NotMapped => write!(formatter, "page is not mapped"),
            Self::OutOfMemory => write!(formatter, "page-table allocation failed"),
            Self::InvalidFlags => write!(formatter, "invalid RISC-V PTE flags"),
            Self::InvalidPageTable => write!(formatter, "invalid Sv39 page-table structure"),
        }
    }
}

impl core::error::Error for PageTableError {}

#[repr(transparent)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct PageTableEntry(usize);

const _: () = assert!(core::mem::size_of::<PageTableEntry>() == core::mem::size_of::<usize>());

impl PageTableEntry {
    fn new(physical_page: usize, flags: RiscvPteFlags) -> Self {
        Self((physical_page << PTE_FLAGS_WIDTH) | flags.bits() as usize)
    }

    fn empty() -> Self {
        Self(0)
    }

    fn flags(self) -> RiscvPteFlags {
        RiscvPteFlags::from_bits_truncate(self.0 as u8)
    }

    pub(crate) fn permissions(self) -> PagePermissions {
        pte::decode(self.flags())
    }

    pub(crate) fn physical_page(self) -> usize {
        self.0 >> PTE_FLAGS_WIDTH
    }

    fn is_valid(self) -> bool {
        self.flags().contains(RiscvPteFlags::V)
    }

    fn is_leaf(self) -> bool {
        let flags = self.flags();
        self.is_valid()
            && flags.intersects(RiscvPteFlags::R | RiscvPteFlags::W | RiscvPteFlags::X)
            && (!flags.contains(RiscvPteFlags::W) || flags.contains(RiscvPteFlags::R))
    }

    fn is_next_table(self) -> bool {
        self.is_valid()
            && !self
                .flags()
                .intersects(RiscvPteFlags::R | RiscvPteFlags::W | RiscvPteFlags::X)
    }
}

/// @description Sv39 page-table mechanism parameterized by a static frame owner adapter。
pub(crate) struct PageTable<Page: TablePage> {
    root_page: usize,
    table_pages: Vec<Page>,
}

impl<Page: TablePage> PageTable<Page> {
    pub(crate) fn try_new() -> Result<Self, PageTableError> {
        let mut table_pages = Vec::new();
        table_pages
            .try_reserve_exact(1)
            .map_err(|_| PageTableError::OutOfMemory)?;
        let root = Page::allocate().ok_or(PageTableError::OutOfMemory)?;
        let root_page = root.physical_page();
        table_pages.push(root);
        Ok(Self {
            root_page,
            table_pages,
        })
    }

    pub(crate) fn token(&self) -> AddressSpaceToken {
        AddressSpaceToken::from_root_page(self.root_page)
    }

    fn allocate_table(&mut self) -> Result<usize, PageTableError> {
        self.table_pages
            .try_reserve(1)
            .map_err(|_| PageTableError::OutOfMemory)?;
        let page = Page::allocate().ok_or(PageTableError::OutOfMemory)?;
        let physical_page = page.physical_page();
        self.table_pages.push(page);
        Ok(physical_page)
    }

    fn read_entry(table_page: usize, index: usize) -> PageTableEntry {
        assert!(index < 512, "Sv39 table index exceeds one page");
        let pointer = (table_page << PAGE_SHIFT) as *const PageTableEntry;
        // SAFETY: table page is retained by Page owner storage and index is bounded to 512 entries.
        unsafe { pointer.add(index).read_volatile() }
    }

    fn write_entry(table_page: usize, index: usize, entry: PageTableEntry) {
        assert!(index < 512, "Sv39 table index exceeds one page");
        let pointer = (table_page << PAGE_SHIFT) as *mut PageTableEntry;
        // SAFETY: caller has exclusive PageTable access; hardware walker may concurrently read,
        // therefore the update is volatile and becomes visible through the caller's TLB fence.
        unsafe { pointer.add(index).write_volatile(entry) };
    }

    fn find_or_create(&mut self, virtual_page: usize) -> Result<(usize, usize), PageTableError> {
        let indexes = sv39::indexes(virtual_page);
        let mut table_page = self.root_page;
        for (level, index) in indexes.into_iter().enumerate() {
            if level == 2 {
                return Ok((table_page, index));
            }
            let mut entry = Self::read_entry(table_page, index);
            if !entry.is_valid() {
                let child = self.allocate_table()?;
                entry = PageTableEntry::new(child, RiscvPteFlags::V);
                Self::write_entry(table_page, index, entry);
            } else if !entry.is_next_table() {
                return Err(PageTableError::InvalidPageTable);
            }
            table_page = entry.physical_page();
        }
        Err(PageTableError::InvalidPageTable)
    }

    fn find(&self, virtual_page: usize) -> Option<PageTableEntry> {
        let indexes = sv39::indexes(virtual_page);
        let mut table_page = self.root_page;
        for (level, index) in indexes.into_iter().enumerate() {
            let entry = Self::read_entry(table_page, index);
            if level == 2 {
                return Some(entry);
            }
            if !entry.is_next_table() {
                return None;
            }
            table_page = entry.physical_page();
        }
        None
    }

    pub(crate) fn reserve(&mut self, virtual_page: usize) -> Result<(), PageTableError> {
        let (table, index) = self.find_or_create(virtual_page)?;
        if Self::read_entry(table, index).is_valid() {
            return Err(PageTableError::AlreadyMapped);
        }
        Ok(())
    }

    pub(crate) fn map(
        &mut self,
        virtual_page: usize,
        physical_page: usize,
        permissions: PagePermissions,
    ) -> Result<(), PageTableError> {
        let flags = pte::encode(permissions).ok_or(PageTableError::InvalidFlags)?;
        let (table, index) = self.find_or_create(virtual_page)?;
        if Self::read_entry(table, index).is_valid() {
            return Err(PageTableError::AlreadyMapped);
        }
        Self::write_entry(table, index, PageTableEntry::new(physical_page, flags));
        Ok(())
    }

    pub(crate) fn unmap(&mut self, virtual_page: usize) -> Result<(), PageTableError> {
        let indexes = sv39::indexes(virtual_page);
        let mut table = self.root_page;
        for index in &indexes[..2] {
            let entry = Self::read_entry(table, *index);
            if !entry.is_next_table() {
                return Err(PageTableError::NotMapped);
            }
            table = entry.physical_page();
        }
        let index = indexes[2];
        if !Self::read_entry(table, index).is_valid() {
            return Err(PageTableError::NotMapped);
        }
        Self::write_entry(table, index, PageTableEntry::empty());
        Ok(())
    }

    pub(crate) fn set_flags(
        &mut self,
        virtual_page: usize,
        permissions: PagePermissions,
    ) -> Result<(), PageTableError> {
        let flags = pte::encode(permissions).ok_or(PageTableError::InvalidFlags)?;
        let indexes = sv39::indexes(virtual_page);
        let mut table = self.root_page;
        for index in &indexes[..2] {
            let entry = Self::read_entry(table, *index);
            if !entry.is_next_table() {
                return Err(PageTableError::NotMapped);
            }
            table = entry.physical_page();
        }
        let index = indexes[2];
        let old = Self::read_entry(table, index);
        if !old.is_leaf() {
            return Err(PageTableError::NotMapped);
        }
        Self::write_entry(
            table,
            index,
            PageTableEntry::new(old.physical_page(), flags),
        );
        Ok(())
    }

    pub(crate) fn translate(&self, virtual_page: usize) -> Option<PageTableEntry> {
        self.find(virtual_page)
            .filter(|entry| entry.is_valid() && entry.is_leaf())
    }
}
