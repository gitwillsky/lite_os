use crate::memory::page_table::PageTableEntry;

use super::config::{self};
use core::fmt::Debug;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysicalAddress(usize);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtualAddress(usize);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PhysicalPageNumber(usize);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VirtualPageNumber(usize);

impl Debug for PhysicalAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("PA:{:#x}", self.0))
    }
}

impl Debug for VirtualAddress {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("VA:{:#x}", self.0))
    }
}

impl Debug for PhysicalPageNumber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("PPN:{:#x}", self.0))
    }
}

impl Debug for VirtualPageNumber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("VPN:{:#x}", self.0))
    }
}

impl From<usize> for PhysicalAddress {
    fn from(addr: usize) -> Self {
        // 仅低位有效
        PhysicalAddress(addr & ((1usize << config::PHYSICAL_ADDRESS_WIDTH) - 1))
    }
}

impl From<usize> for VirtualAddress {
    fn from(addr: usize) -> Self {
        // 仅低位有效
        VirtualAddress(addr & ((1usize << config::VIRTUAL_ADDRESS_WIDTH) - 1))
    }
}

impl From<PhysicalAddress> for usize {
    fn from(addr: PhysicalAddress) -> Self {
        addr.0
    }
}

impl From<VirtualAddress> for usize {
    fn from(addr: VirtualAddress) -> Self {
        if addr.0 >= ((1 << config::VIRTUAL_ADDRESS_WIDTH) - 1) {
            addr.0 | (!(1 << config::VIRTUAL_ADDRESS_WIDTH) - 1)
        } else {
            addr.0
        }
    }
}

impl From<PhysicalPageNumber> for usize {
    fn from(value: PhysicalPageNumber) -> Self {
        value.0
    }
}

impl From<VirtualPageNumber> for usize {
    fn from(value: VirtualPageNumber) -> Self {
        value.0
    }
}

impl From<usize> for PhysicalPageNumber {
    fn from(addr: usize) -> Self {
        Self(addr & ((1 << config::PPN_WIDTH) - 1))
    }
}

impl From<usize> for VirtualPageNumber {
    fn from(addr: usize) -> Self {
        Self(addr & ((1 << config::VPN_WIDTH) - 1))
    }
}

impl PhysicalAddress {
    pub fn page_offset(&self) -> usize {
        self.0 & (config::PAGE_SIZE - 1)
    }

    pub fn floor(&self) -> PhysicalPageNumber {
        PhysicalPageNumber(self.0 / config::PAGE_SIZE)
    }

    pub fn ceil(&self) -> PhysicalPageNumber {
        if self.0 == 0 {
            PhysicalPageNumber(0)
        } else {
            PhysicalPageNumber((self.0 + config::PAGE_SIZE - 1) / config::PAGE_SIZE)
        }
    }

    pub fn is_aligned(&self) -> bool {
        self.page_offset() == 0
    }

    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn get_mut<T>(&self) -> &'static mut T {
        unsafe { (self.0 as *mut T).as_mut().unwrap() }
    }
}

impl VirtualAddress {
    pub fn page_offset(&self) -> usize {
        self.0 & (config::PAGE_SIZE - 1)
    }

    pub fn is_aligned(&self) -> bool {
        self.page_offset() == 0
    }

    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn ceil(&self) -> VirtualPageNumber {
        if self.0 == 0 {
            VirtualPageNumber(0)
        } else {
            VirtualPageNumber((self.0 - 1 + config::PAGE_SIZE) / config::PAGE_SIZE)
        }
    }

    pub fn floor(&self) -> VirtualPageNumber {
        VirtualPageNumber(self.0 / config::PAGE_SIZE)
    }
}

impl From<PhysicalPageNumber> for PhysicalAddress {
    fn from(ppn: PhysicalPageNumber) -> Self {
        PhysicalAddress(ppn.0 * config::PAGE_SIZE)
    }
}

impl From<PhysicalAddress> for PhysicalPageNumber {
    fn from(value: PhysicalAddress) -> Self {
        assert!(value.is_aligned());
        value.floor()
    }
}

impl From<VirtualAddress> for VirtualPageNumber {
    fn from(value: VirtualAddress) -> Self {
        assert!(value.is_aligned());
        value.floor()
    }
}

impl From<VirtualPageNumber> for VirtualAddress {
    fn from(value: VirtualPageNumber) -> Self {
        VirtualAddress(value.0 * config::PAGE_SIZE)
    }
}

impl PhysicalPageNumber {
    pub fn get_bytes_array_mut(self) -> &'static mut [u8] {
        let pa: PhysicalAddress = self.into();
        unsafe { core::slice::from_raw_parts_mut(pa.0 as *mut u8, config::PAGE_SIZE) }
    }

    pub fn get_pte_array(self) -> &'static mut [PageTableEntry] {
        let pa: PhysicalAddress = self.into();
        unsafe { core::slice::from_raw_parts_mut(pa.0 as *mut PageTableEntry, 512) }
    }

    pub fn get_mut<T>(self) -> &'static mut T {
        let pa: PhysicalAddress = self.into();
        unsafe { (pa.0 as *mut T).as_mut().unwrap() }
    }

    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn add_one(&self) -> Self {
        PhysicalPageNumber(self.0 + 1)
    }
}

impl VirtualPageNumber {
    pub fn from_vpn(vpn: usize) -> Self {
        VirtualPageNumber(vpn)
    }

    // 获取页号
    pub fn indexes(&self) -> [usize; 3] {
        let mut vpn = self.0;
        let mut indexes = [0usize; 3];
        for i in (0..3).rev() {
            indexes[i] = vpn & 511;
            vpn >>= 9;
        }
        indexes
    }

    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn next(&self) -> Self {
        VirtualPageNumber(self.0 + 1)
    }
}
