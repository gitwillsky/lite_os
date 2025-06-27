use alloc::vec;
use alloc::vec::Vec;
use bitflags::bitflags;

use crate::memory::{address::VirtualPageNumber, frame_allocator::alloc};

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

    pub fn token(&self) -> usize {
        self.root_ppn.as_usize() | 8usize << 60
    }

    pub fn from_token(satp_val: usize) -> Self {
        Self {
            root_ppn: PhysicalPageNumber::from(satp_val),
            entries: vec![],
        }
    }

    fn new_pte(&mut self, flags: Option<PTEFlags>) -> PageTableEntry {
        let frame = alloc().unwrap();
        let ppn = frame.ppn;
        self.entries.push(frame);
        PageTableEntry::new(ppn, flags.unwrap_or(PTEFlags::V))
    }

    fn find_pte_create(&mut self, vpn: VirtualPageNumber) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<_> = None;

        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
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

    fn find_pte(&self, vpn: VirtualPageNumber) -> Option<&PageTableEntry> {
        let idxs = vpn.indexes();

        let mut ppn = self.root_ppn;
        let mut result: Option<&PageTableEntry> = None;

        for (i, idx) in idxs.iter().enumerate() {
            let pte = &ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
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
        assert!(!pte.is_valid(), "vpn {:?} is already mapped", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
    }

    pub fn unmap(&mut self, vpn: VirtualPageNumber) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }

    pub fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
}
