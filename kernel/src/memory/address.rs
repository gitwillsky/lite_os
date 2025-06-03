use super::config::{self, PHYSICAL_ADDRESS_WIDTH, PPN_WIDTH, VPN_WIDTH};
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
        PhysicalAddress(addr & ((1usize << PHYSICAL_ADDRESS_WIDTH) - 1))
    }
}

impl From<PhysicalAddress> for usize {
    fn from(addr: PhysicalAddress) -> Self {
        addr.0
    }
}

impl From<VirtualAddress> for usize {
    fn from(addr: VirtualAddress) -> Self {
        addr.0
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

impl From<usize> for VirtualAddress {
    fn from(addr: usize) -> Self {
        VirtualAddress(addr)
    }
}

impl From<usize> for PhysicalPageNumber {
    fn from(addr: usize) -> Self {
        PhysicalPageNumber(addr & ((1usize << PPN_WIDTH) - 1))
    }
}

impl From<usize> for VirtualPageNumber {
    fn from(addr: usize) -> Self {
        VirtualPageNumber(addr & ((1usize << VPN_WIDTH) - 1))
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
        PhysicalPageNumber((self.0 + config::PAGE_SIZE - 1) / config::PAGE_SIZE)
    }

    pub fn is_aligned(&self) -> bool {
        self.0 % config::PAGE_SIZE == 0
    }
}

impl VirtualAddress {
    pub fn is_aligned(&self) -> bool {
        self.0 % config::PAGE_SIZE == 0
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

impl PhysicalPageNumber {
    pub fn get_bytes_mut(&self) -> &'static mut [u8; config::PAGE_SIZE] {
        unsafe {
            ((self.0 * config::PAGE_SIZE) as *mut [u8; config::PAGE_SIZE])
                .as_mut()
                .unwrap()
        }
    }
}
