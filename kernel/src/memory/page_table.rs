use alloc::vec;
use alloc::vec::Vec;
use bitflags::bitflags;

use crate::memory::{
    address::{PhysicalAddress, VirtualPageNumber},
    frame_allocator::alloc,
};

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
        (self.0 >> PTE_FLAGS_WIDTH).into()
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
        let frame_tracker =
            frame_allocator::alloc().expect("can not alloc new frame to create PageTable");
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

    fn new_pte(&mut self, flags: Option<PTEFlags>) -> PageTableEntry {
        let frame = alloc().unwrap();
        self.entries.push(frame);
        PageTableEntry::new(frame.ppn, flags.unwrap_or(PTEFlags::V))
    }

    fn find_pte_create(&mut self, vpn: VirtualPageNumber) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<_> = None;

        for i in 0..3 {
            let pte = ppn.get_pte_array()[idxs[i]];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                *pte = self.new_pte(None);
            }
            ppn = pte.ppn();
        }
        result
    }

    fn find_pte(&self, vpn: VirtualPageNumber) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();

        let mut ppn = self.root_ppn;
        let mut result: Option<_> = None;

        for i in 0..3 {
            let pte = ppn.get_pte_array()[idxs[i]];
            if i == 2 {
                result = Some(result);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }

        result
    }

    pub fn map(&mut self, vpn: VirtualPageNumber, ppn: PhysicalPageNumber, flags: PTEFlags) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags)
    }

    pub fn unmap(&mut self, vpn: VirtualPageNumber) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }

    pub fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        let pte = self.find_pte(vpn);
        pte.map(|pte| pte.clone())
    }
}
