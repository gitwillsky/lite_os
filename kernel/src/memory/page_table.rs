use bitflags::bitflags;

use super::{address::PhysicalPageNumber, config};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct PTEFlags: u64 {
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

// PPN 在 PTE 中的左位移
const PPN_SHIFT: usize = 10;
const FLAGS_MASK: u64 = 0x3FF; // Bits 0-9 for flags
const PPN_MASK: usize = 0xFFFFFFFFFF; // 44 bits for PPN

#[derive(Copy, Clone, Debug)]
#[repr(transparent)] // 确保内存布局与 u64 完全相同
pub struct PageTableEntry(u64);

impl PageTableEntry {
    fn calc_pte(ppn: PhysicalPageNumber, flags: PTEFlags) -> u64 {
        (ppn.as_usize()  << PPN_SHIFT) as u64 | (flags.bits() & FLAGS_MASK)
    }

    pub fn new(ppn: PhysicalPageNumber, flags: PTEFlags) -> Self {
        Self(PageTableEntry::calc_pte(ppn, flags))
    }

    pub fn reset(&mut self, ppn: PhysicalPageNumber, flags: PTEFlags) {
        self.0 = PageTableEntry::calc_pte(ppn, flags)
    }

    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits_truncate(self.0 & FLAGS_MASK)
    }

    pub fn ppn(&self) -> PhysicalPageNumber {
        (self.0 as usize >> PPN_SHIFT & PPN_MASK).into()
    }

    pub fn is_valid(&self) -> bool {
        self.flags().contains(PTEFlags::V)
    }

    /// 判断是否为叶子节点，指向物理页帧
    pub fn is_leaf(&self) -> bool {
        self.is_valid()
            && self
                .flags()
                .intersects(PTEFlags::X | PTEFlags::W | PTEFlags::R)
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

const PTE_COUNTER_PER_TABLE: usize = 2 ^ 9; // sv39 下每个页表地址空间有 9 bit

#[repr(align(4096))]
#[derive(Debug)]
pub struct PageTable {
    entries: [PageTableEntry; PTE_COUNTER_PER_TABLE],
}

impl PageTable {
    pub fn new() -> Self {
        Self {
            entries: [PageTableEntry(0); PTE_COUNTER_PER_TABLE],
        }
    }

    pub fn get_pte(&self, index: usize) -> Option<PageTableEntry> {
        self.entries.get(index).copied()
    }

    pub fn set_pte(&mut self, index: usize, pte: PageTableEntry) {
        assert!(
            index < PTE_COUNTER_PER_TABLE,
            "set pte failed, invalid index"
        );
        self.entries[index] = pte;
    }

    pub fn get_pte_mut(&mut self, index: usize) -> Option<&mut PageTableEntry> {
        self.entries.get_mut(index)
    }
}
