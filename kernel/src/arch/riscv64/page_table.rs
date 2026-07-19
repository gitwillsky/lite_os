use super::{
    mmu::{
        AddressSpaceToken, allocate_address_space_id, release_address_space_id_after_global_fence,
    },
    pte::{self, PagePermissions, RiscvPteFlags},
    sv39,
};
use crate::fallible_tree::{FallibleMap, VacantEntry};

const PTE_FLAGS_WIDTH: usize = 10;
const PAGE_SHIFT: usize = 12;
const SV39_LEVELS: usize = 3;

/// Sv39 root role；generic owner 必须显式声明 kernel 或 user 用途。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddressSpaceKind {
    Kernel,
    User,
}

/// @description Architecture page-table page allocation seam。
///
/// Implementor owns physical-frame policy and lifetime; the Sv39 walker owns only table layout。
pub(crate) trait TablePage: Sized {
    fn allocate() -> Option<Self>;
    fn physical_page(&self) -> usize;
}

/// @description leaf revoke 时从 active page-table topology 摘除、等待 fence 的 table owners。
pub(crate) struct RetiredTablePages<Page> {
    entries: [Option<VacantEntry<usize, Page>>; 2],
}

impl<Page> RetiredTablePages<Page> {
    fn new() -> Self {
        Self {
            entries: [None, None],
        }
    }

    fn push(&mut self, entry: VacantEntry<usize, Page>) {
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("Sv39 unmap retired more than two table levels");
        *slot = Some(entry);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }
}

impl<Page> IntoIterator for RetiredTablePages<Page> {
    type Item = VacantEntry<usize, Page>;
    type IntoIter = core::iter::Flatten<core::array::IntoIter<Option<Self::Item>, 2>>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter().flatten()
    }
}

/// @description 一次 leaf revoke 的完整 virtual span 与 fence-retained table owners。
pub(crate) struct Unmapped<Page> {
    first_page: usize,
    page_count: usize,
    retired: RetiredTablePages<Page>,
}

impl<Page> Unmapped<Page> {
    pub(crate) fn into_parts(self) -> (usize, usize, RetiredTablePages<Page>) {
        (self.first_page, self.page_count, self.retired)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageTableError {
    AlreadyMapped,
    NotMapped,
    OutOfMemory,
    InvalidFlags,
    InvalidPageTable,
    AddressSpaceIdentifiersExhausted,
}

impl core::fmt::Display for PageTableError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::AlreadyMapped => write!(formatter, "page is already mapped"),
            Self::NotMapped => write!(formatter, "page is not mapped"),
            Self::OutOfMemory => write!(formatter, "page-table allocation failed"),
            Self::InvalidFlags => write!(formatter, "invalid RISC-V PTE flags"),
            Self::InvalidPageTable => write!(formatter, "invalid Sv39 page-table structure"),
            Self::AddressSpaceIdentifiersExhausted => {
                write!(formatter, "RISC-V address-space identifiers exhausted")
            }
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
        RiscvPteFlags::from_bits_truncate(self.0 as u16)
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
    table_pages: FallibleMap<usize, Page>,
    address_space_id: usize,
    kind: AddressSpaceKind,
}

impl<Page: TablePage> PageTable<Page> {
    pub(crate) fn try_new(kind: AddressSpaceKind) -> Result<Self, PageTableError> {
        let root = Page::allocate().ok_or(PageTableError::OutOfMemory)?;
        let root_page = root.physical_page();
        let mut table_pages = FallibleMap::new();
        assert!(
            table_pages
                .try_insert(root_page, root)
                .map_err(|_| PageTableError::OutOfMemory)?
                .is_none(),
            "root page-table frame identity collided"
        );
        let address_space_id =
            allocate_address_space_id().ok_or(PageTableError::AddressSpaceIdentifiersExhausted)?;
        Ok(Self {
            root_page,
            table_pages,
            address_space_id,
            kind,
        })
    }

    pub(crate) fn token(&self) -> AddressSpaceToken {
        AddressSpaceToken::from_root_page(self.root_page, self.address_space_id)
    }

    /// @description 激活 RISC-V kernel Sv39 root；该 backend 保持单 root 契约。
    pub(crate) fn activate_kernel(&self) {
        assert_eq!(self.kind, AddressSpaceKind::Kernel);
        super::mmu::activate_kernel(self.token());
    }

    /// @description 返回 RISC-V user trap 切回 kernel root 所需 token。
    pub(crate) fn kernel_trap_token(&self) -> super::mmu::KernelTrapToken {
        assert_eq!(self.kind, AddressSpaceKind::Kernel);
        self.token()
    }

    /// @description 在 generic owner 已完成 local/remote 全量 fence 后退休 ASID。
    pub(crate) fn release_address_space_id_after_global_fence(&mut self) {
        assert_ne!(self.address_space_id, 0, "page-table ASID retired twice");
        release_address_space_id_after_global_fence(self.address_space_id);
        self.address_space_id = 0;
    }

    fn allocate_table(&mut self) -> Result<usize, PageTableError> {
        let page = Page::allocate().ok_or(PageTableError::OutOfMemory)?;
        let physical_page = page.physical_page();
        let entry = self
            .table_pages
            .try_prepare_vacant(physical_page, page)
            .map_err(|_| PageTableError::OutOfMemory)?;
        self.table_pages.commit_vacant(entry);
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

    fn find_or_create_level(
        &mut self,
        virtual_page: usize,
        target_level: usize,
    ) -> Result<(usize, usize), PageTableError> {
        assert!(target_level < SV39_LEVELS);
        let indexes = sv39::indexes(virtual_page);
        let mut table_page = self.root_page;
        for level in 0..target_level {
            let index = indexes[level];
            let entry = Self::read_entry(table_page, index);
            if entry.is_next_table() {
                table_page = entry.physical_page();
                continue;
            }
            if entry.is_valid() {
                return Err(PageTableError::InvalidPageTable);
            }
            // Missing suffix 的全部 page/node owners 在首个 parent PTE publication 前准备。
            // 若任一分配失败，active map 精确回滚，hardware topology 保持原样。
            let missing = target_level - level;
            let mut pages = [None, None];
            for slot in pages.iter_mut().take(missing) {
                match self.allocate_table() {
                    Ok(page) => *slot = Some(page),
                    Err(error) => {
                        for page in pages.into_iter().flatten() {
                            drop(
                                self.table_pages
                                    .remove(&page)
                                    .expect("unpublished table page lost owner"),
                            );
                        }
                        return Err(error);
                    }
                }
            }
            let mut parent = table_page;
            for (offset, child) in pages.into_iter().flatten().enumerate() {
                Self::write_entry(
                    parent,
                    indexes[level + offset],
                    PageTableEntry::new(child, RiscvPteFlags::V),
                );
                parent = child;
            }
            return Ok((parent, indexes[target_level]));
        }
        Ok((table_page, indexes[target_level]))
    }

    fn find_or_create(&mut self, virtual_page: usize) -> Result<(usize, usize), PageTableError> {
        self.find_or_create_level(virtual_page, 2)
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
        self.map_leaf(virtual_page, physical_page, permissions, 2)
    }

    fn leaf_span(level: usize) -> usize {
        1usize << (9 * (2 - level))
    }

    fn largest_contiguous_leaf(
        virtual_page: usize,
        physical_page: usize,
        remaining: usize,
    ) -> usize {
        (0..SV39_LEVELS)
            .find(|level| {
                let span = Self::leaf_span(*level);
                virtual_page.is_multiple_of(span)
                    && physical_page.is_multiple_of(span)
                    && remaining >= span
            })
            .unwrap_or(2)
    }

    fn map_leaf(
        &mut self,
        virtual_page: usize,
        physical_page: usize,
        permissions: PagePermissions,
        level: usize,
    ) -> Result<(), PageTableError> {
        let span = Self::leaf_span(level);
        if !virtual_page.is_multiple_of(span) || !physical_page.is_multiple_of(span) {
            return Err(PageTableError::InvalidPageTable);
        }
        let flags = pte::encode(permissions).ok_or(PageTableError::InvalidFlags)?;
        let (table, index) = self.find_or_create_level(virtual_page, level)?;
        if Self::read_entry(table, index).is_valid() {
            return Err(PageTableError::AlreadyMapped);
        }
        Self::write_entry(table, index, PageTableEntry::new(physical_page, flags));
        Ok(())
    }

    /// @description 用最大 leaf 映射等长、物理连续的 Sv39 region。
    pub(crate) fn map_contiguous_range(
        &mut self,
        virtual_start_page: usize,
        physical_start_page: usize,
        page_count: usize,
        permissions: PagePermissions,
    ) -> Result<(), PageTableError> {
        if page_count == 0 {
            return Err(PageTableError::InvalidPageTable);
        }
        let mut mapped = 0;
        while mapped < page_count {
            let virtual_page = virtual_start_page + mapped;
            let physical_page = physical_start_page + mapped;
            let level =
                Self::largest_contiguous_leaf(virtual_page, physical_page, page_count - mapped);
            self.map_leaf(virtual_page, physical_page, permissions, level)?;
            mapped += Self::leaf_span(level);
        }
        Ok(())
    }

    fn table_is_empty(table_page: usize) -> bool {
        (0..512).all(|index| !Self::read_entry(table_page, index).is_valid())
    }

    pub(crate) fn unmap(&mut self, virtual_page: usize) -> Result<Unmapped<Page>, PageTableError> {
        let indexes = sv39::indexes(virtual_page);
        let mut tables = [self.root_page; 3];
        for level in 0..SV39_LEVELS {
            let entry = Self::read_entry(tables[level], indexes[level]);
            if !entry.is_valid() {
                return Err(PageTableError::NotMapped);
            }
            if entry.is_leaf() {
                let span = Self::leaf_span(level);
                let first_page = virtual_page & !(span - 1);
                if virtual_page != first_page {
                    return Err(PageTableError::NotMapped);
                }
                Self::write_entry(tables[level], indexes[level], PageTableEntry::empty());
                let mut retired = RetiredTablePages::new();
                for child_level in (1..=level).rev() {
                    if !Self::table_is_empty(tables[child_level]) {
                        break;
                    }
                    Self::write_entry(
                        tables[child_level - 1],
                        indexes[child_level - 1],
                        PageTableEntry::empty(),
                    );
                    retired.push(
                        self.table_pages
                            .take_entry(&tables[child_level])
                            .expect("empty table lost owner"),
                    );
                }
                return Ok(Unmapped {
                    first_page,
                    page_count: span,
                    retired,
                });
            }
            if level == 2 || !entry.is_next_table() {
                return Err(PageTableError::InvalidPageTable);
            }
            tables[level + 1] = entry.physical_page();
        }
        Err(PageTableError::InvalidPageTable)
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
        let indexes = sv39::indexes(virtual_page);
        let mut table_page = self.root_page;
        for (level, index) in indexes.into_iter().enumerate() {
            let entry = Self::read_entry(table_page, index);
            if entry.is_leaf() {
                let offset = virtual_page & (Self::leaf_span(level) - 1);
                return Some(PageTableEntry::new(
                    entry.physical_page() + offset,
                    entry.flags(),
                ));
            }
            if !entry.is_next_table() {
                return None;
            }
            table_page = entry.physical_page();
        }
        None
    }

    #[cfg(test)]
    pub(crate) fn table_page_count(&self) -> usize {
        self.table_pages.len()
    }
}
