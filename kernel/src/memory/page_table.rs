use alloc::vec;
use alloc::vec::Vec;
use bitflags::bitflags;

use crate::memory::address::VirtualPageNumber;

use super::{
    address::PhysicalPageNumber,
    config::PTE_FLAGS_WIDTH,
    frame_allocator::{self, FrameTracker},
};

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
        Self(usize::from(ppn) << PTE_FLAGS_WIDTH & flags.bits() as usize)
    }

    pub fn empty() -> Self {
        Self(0)
    }

    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.0 as u8).unwrap()
    }

    pub fn ppn(&self) -> PhysicalPageNumber {
        (self.0 as usize >> PTE_FLAGS_WIDTH).into()
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

#[derive(Debug)]
pub struct PageTable {
    root_ppn: PhysicalPageNumber,
    entries: Vec<FrameTracker>,
}

impl PageTable {
    pub fn new() -> Self {
        let frame_tracker = frame_allocator::alloc().expect("can not alloc new frame");
        Self {
            root_ppn: frame_tracker.ppn,
            entries: vec![frame_tracker],
        }
    }

    pub fn from_token(satp_val: usize) -> Self {
        Self {
            root_ppn: PhysicalPageNumber::from(satp_val),
            entries: Vec::new(),
        }
    }

    pub fn map(&mut self, vpn: VirtualPageNumber, ppn: PhysicalPageNumber, flags: PTEFlags) {

    }

    pub fn unmap(&mut self, vpn: VirtualPageNumber) {

    }
}
