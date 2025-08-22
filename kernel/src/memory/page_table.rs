use alloc::vec::Vec;
use alloc::{string::String, vec};
use bitflags::bitflags;

use crate::memory::address::PhysicalAddress;
use crate::memory::{
    address::{VirtualAddress, VirtualPageNumber},
    frame_allocator::alloc,
};

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
}

impl core::fmt::Display for PageTableError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            PageTableError::AlreadyMapped => write!(f, "Page is already mapped"),
            PageTableError::NotMapped => write!(f, "Page is not mapped"),
            PageTableError::OutOfMemory => write!(f, "Out of memory"),
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
            root_ppn: PhysicalPageNumber::from(satp_val & ((1 << 44) - 1)),
            entries: vec![],
        }
    }

    fn new_pte(&mut self, flags: Option<PTEFlags>) -> PageTableEntry {
        let frame = alloc().expect("can not alloc new frame to create PageTableEntry");
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

    fn find_pte(&self, vpn: VirtualPageNumber) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();

        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;

        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
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

    pub fn map(
        &mut self,
        vpn: VirtualPageNumber,
        ppn: PhysicalPageNumber,
        flags: PTEFlags,
    ) -> Result<(), PageTableError> {
        let pte = self
            .find_pte_create(vpn)
            .ok_or(PageTableError::OutOfMemory)?;
        if pte.is_valid() {
            return Err(PageTableError::AlreadyMapped);
        }
        // 默认为新映射设置 Accessed 位；若可写则同时设置 Dirty 位，避免在不支持硬件 A/D 位的环境中出现首次访问陷入
        let mut final_flags = flags | PTEFlags::V | PTEFlags::A;
        if flags.contains(PTEFlags::W) {
            final_flags |= PTEFlags::D;
        }
        *pte = PageTableEntry::new(ppn, final_flags);
        Ok(())
    }

    pub fn unmap(&mut self, vpn: VirtualPageNumber) -> Result<(), PageTableError> {
        let pte = self.find_pte(vpn).ok_or(PageTableError::NotMapped)?;
        if !pte.is_valid() {
            return Err(PageTableError::NotMapped);
        }
        *pte = PageTableEntry::empty();
        Ok(())
    }

    pub fn translate(&self, vpn: VirtualPageNumber) -> Option<PageTableEntry> {
        self.find_pte(vpn)
            .and_then(|pte| if pte.is_valid() { Some(*pte) } else { None })
    }

    pub fn translate_va(&self, va: VirtualAddress) -> Option<PhysicalAddress> {
        self.find_pte(va.clone().floor()).and_then(|pte| {
            if pte.is_valid() {
                let aligned_pa: PhysicalAddress = pte.ppn().into();
                let offset = va.page_offset();
                let aligned_pa_usize: usize = aligned_pa.into();
                Some((aligned_pa_usize + offset).into())
            } else {
                None
            }
        })
    }
}

/// translate a pointer to a mutable u8 Vec through page table
pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize) -> Vec<&'static mut [u8]> {
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = start + len;
    let mut v = Vec::new();
    while start < end {
        let start_va = VirtualAddress::from(start);
        let vpn = start_va.floor();
        let ppn = page_table
            .translate(vpn)
            .expect("Page table entry not found in translated_byte_buffer")
            .ppn();
        let next_vpn = vpn.next();
        let mut end_va: VirtualAddress = next_vpn.into();
        end_va = end_va.min(VirtualAddress::from(end));
        if end_va.page_offset() == 0 {
            v.push(&mut ppn.get_bytes_array_mut()[start_va.page_offset()..]);
        } else {
            v.push(&mut ppn.get_bytes_array_mut()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    v
}

pub fn translated_str(token: usize, ptr: *const u8) -> String {
    let page_table = PageTable::from_token(token);
    let mut string = String::new();
    let mut va = ptr as usize;
    loop {
        let v_addr: VirtualAddress = va.into();
        let ch: u8 = *(page_table
            .translate_va(v_addr)
            .expect("Page table entry not found in translated_str")
            .get_mut());
        if ch == 0 {
            break;
        } else {
            string.push(ch as char);
            va += 1
        }
    }
    string
}

pub fn translated_ref_mut<T>(token: usize, ptr: *mut T) -> &'static mut T
where
    T: Sized,
{
    let page_table = PageTable::from_token(token);
    let va = ptr as usize;
    page_table
        .translate_va(VirtualAddress::from(va))
        .expect("Page table entry not found in translated_ref_mut")
        .get_mut()
}
