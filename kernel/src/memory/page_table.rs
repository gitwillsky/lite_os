use alloc::vec;
use alloc::vec::Vec;
use bitflags::bitflags;

use crate::memory::{address::VirtualPageNumber, frame_allocator::alloc};

use super::{
    address::PhysicalPageNumber,
    config::PTE_FLAGS_WIDTH,
    frame_allocator::{self, FrameTracker},
};

#[derive(Debug, Clone, Copy)]
pub enum PageTableError {
    AlreadyMapped,
    NotMapped,
    OutOfMemory,
    InvalidFlags,
    InvalidPageTable,
}

impl core::fmt::Display for PageTableError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            PageTableError::AlreadyMapped => write!(f, "Page is already mapped"),
            PageTableError::NotMapped => write!(f, "Page is not mapped"),
            PageTableError::OutOfMemory => write!(f, "Out of memory"),
            PageTableError::InvalidFlags => write!(f, "Invalid page table flags"),
            PageTableError::InvalidPageTable => write!(f, "Invalid page table structure"),
        }
    }
}

impl core::error::Error for PageTableError {}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PTEFlags: u8 {
        const V = 1 << 0; // Valid
        const R = 1 << 1; // Read
        const W = 1 << 2; // Write
        const X = 1 << 3; // Execute
        const U = 1 << 4; // User Space PTE
        const G = 1 << 5; // Global PTE
        const A = 1 << 6; // Accessed (by Hardware)
        const D = 1 << 7; // Dirty (by Hardware)
    }
}

#[derive(Copy, Clone, Debug)]
#[repr(transparent)] // 确保内存布局与 u64 完全相同
pub struct PageTableEntry(usize);

impl PageTableEntry {
    pub fn new(ppn: PhysicalPageNumber, flags: PTEFlags) -> Self {
        let ppn_val = usize::from(ppn);
        let flags_val = flags.bits() as usize;
        let result = (ppn_val << PTE_FLAGS_WIDTH) | flags_val;
        Self(result)
    }

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.0 as u8).unwrap()
    }

    pub fn ppn(&self) -> PhysicalPageNumber {
        let ppn_val = self.0 >> PTE_FLAGS_WIDTH;
        ppn_val.into()
    }

    pub fn is_valid(&self) -> bool {
        self.flags().contains(PTEFlags::V)
    }

    /// 判断是否为叶子节点，指向物理页帧
    pub fn is_leaf(&self) -> bool {
        let flags = self.flags();
        self.is_valid()
            && flags.intersects(PTEFlags::X | PTEFlags::W | PTEFlags::R)
            && (!flags.contains(PTEFlags::W) || flags.contains(PTEFlags::R))
    }

    fn is_pointer_to_next_table(&self) -> bool {
        self.is_valid()
            && !self
                .flags()
                .intersects(PTEFlags::X | PTEFlags::W | PTEFlags::R)
    }

    pub fn get_next_table_ppn(&self) -> PhysicalPageNumber {
        assert!(self.is_pointer_to_next_table());
        self.ppn()
    }
}

#[derive(Debug)]
pub struct PageTable {
    root_ppn: PhysicalPageNumber,
    entries: Vec<FrameTracker>,
}

impl PageTable {
    pub fn new() -> Self {
        Self::try_new().expect("can not alloc root frame to create kernel PageTable")
    }

    /// @description 分配并初始化一个拥有 root frame 的空 Sv39 页表。
    ///
    /// @return 成功返回页表；物理页耗尽返回 `PageTableError::OutOfMemory`。
    pub fn try_new() -> Result<Self, PageTableError> {
        let frame_tracker = frame_allocator::alloc().ok_or(PageTableError::OutOfMemory)?;
        Ok(Self {
            root_ppn: frame_tracker.ppn,
            entries: vec![frame_tracker],
        })
    }

    pub fn token(&self) -> usize {
        self.root_ppn.as_usize() | 8usize << 60
    }

    fn allocate_table(&mut self) -> Result<PhysicalPageNumber, PageTableError> {
        let frame = alloc().ok_or(PageTableError::OutOfMemory)?;
        let ppn = frame.ppn;
        self.entries.push(frame);
        Ok(ppn)
    }

    fn read_entry(ppn: PhysicalPageNumber, index: usize) -> PageTableEntry {
        let ptr = ppn.as_page_ptr().cast::<PageTableEntry>();
        // SAFETY: 页表页由当前 PageTable 的 root/entries 持有，index 来自 Sv39 的 9-bit
        // 索引，因此位于 512-entry 页内。volatile 访问用于与硬件 page-table walker 交互。
        unsafe { ptr.add(index).read_volatile() }
    }

    fn write_entry(ppn: PhysicalPageNumber, index: usize, entry: PageTableEntry) {
        let ptr = ppn.as_page_mut_ptr().cast::<PageTableEntry>();
        // SAFETY: 调用方持有 &mut PageTable，保证软件写者唯一；ppn 是本页表 walk
        // 已验证的 table page，index 位于 512-entry 页内。硬件可并发读取，后续 fence 生效。
        unsafe { ptr.add(index).write_volatile(entry) }
    }

    fn find_pte_create(
        &mut self,
        vpn: VirtualPageNumber,
    ) -> Result<(PhysicalPageNumber, usize), PageTableError> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;

        for (i, idx) in idxs.iter().enumerate() {
            if i == 2 {
                return Ok((ppn, *idx));
            }
            let mut pte = Self::read_entry(ppn, *idx);
            if !pte.is_valid() {
                let child_ppn = self.allocate_table()?;
                pte = PageTableEntry::new(child_ppn, PTEFlags::V);
                Self::write_entry(ppn, *idx, pte);
            } else if !pte.is_pointer_to_next_table() {
                return Err(PageTableError::InvalidPageTable);
            }
            ppn = pte.ppn();
        }
        Err(PageTableError::InvalidPageTable)
    }

    fn find_pte(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;

        for (i, idx) in idxs.iter().enumerate() {
            let pte = Self::read_entry(ppn, *idx);
            if i == 2 {
                return Some(pte);
            }
            if !pte.is_pointer_to_next_table() {
                return None;
            }
            ppn = pte.ppn();
        }
        None
    }

    pub fn map(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        flags: PTEFlags,
    ) -> Result<(), PageTableError> {
        if flags.contains(PTEFlags::W) && !flags.contains(PTEFlags::R)
            || flags.contains(PTEFlags::W | PTEFlags::X)
        {
            return Err(PageTableError::InvalidFlags);
        }
        let (table_ppn, index) = self.find_pte_create(vpn)?;
        let pte = Self::read_entry(table_ppn, index);
        if pte.is_valid() {
            return Err(PageTableError::AlreadyMapped);
        }
        // 默认为新映射设置 Accessed 位；若可写则同时设置 Dirty 位，避免在不支持硬件 A/D 位的环境中出现首次访问陷入
        let mut final_flags = flags | PTEFlags::V | PTEFlags::A;
        if flags.contains(PTEFlags::W) {
            final_flags |= PTEFlags::D;
        }
        Self::write_entry(table_ppn, index, PageTableEntry::new(ppn, final_flags));
        Ok(())
    }

    pub fn unmap(&mut self, vpn: VirtualPageNumber) -> Result<(), PageTableError> {
        let idxs = vpn.indexes();
        let mut table_ppn = self.root_ppn;
        for idx in &idxs[..2] {
            let pte = Self::read_entry(table_ppn, *idx);
            if !pte.is_pointer_to_next_table() {
                return Err(PageTableError::NotMapped);
            }
            table_ppn = pte.ppn();
        }
        let index = idxs[2];
        if !Self::read_entry(table_ppn, index).is_valid() {
            return Err(PageTableError::NotMapped);
        }
        Self::write_entry(table_ppn, index, PageTableEntry::empty());
        Ok(())
    }

    pub fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.find_pte(vpn)
            .and_then(|pte| (pte.is_valid() && pte.is_leaf()).then_some(pte))
    }
}
