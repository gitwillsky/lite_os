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

impl From<usize> for PhysicalPageNumber {
    fn from(addr: usize) -> Self {
        // 强制页对齐
        PhysicalPageNumber((addr / config::PAGE_SIZE))
    }
}

impl From<usize> for VirtualPageNumber {
    fn from(addr: usize) -> Self {
        VirtualPageNumber(addr / config::PAGE_SIZE)
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

    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn as_kernel_vaddr(&self) -> usize {
        // 恒等映射
        self.0
    }
}

impl VirtualAddress {
    pub fn is_aligned(&self) -> bool {
        self.0 % config::PAGE_SIZE == 0
    }

    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn floor(&self) -> VirtualPageNumber {
        VirtualPageNumber(self.0 / config::PAGE_SIZE)
    }

    pub fn ceil(&self) -> VirtualPageNumber {
        VirtualPageNumber((self.0 + config::PAGE_SIZE - 1) / config::PAGE_SIZE)
    }
}

impl From<&PhysicalPageNumber> for PhysicalAddress {
    fn from(ppn: &PhysicalPageNumber) -> Self {
        PhysicalAddress(ppn.0 * config::PAGE_SIZE)
    }
}

impl From<&PhysicalAddress> for PhysicalPageNumber {
    fn from(value: &PhysicalAddress) -> Self {
        assert!(value.is_aligned());
        value.floor()
    }
}

impl From<&VirtualAddress> for VirtualPageNumber {
    fn from(value: &VirtualAddress) -> Self {
        assert!(value.is_aligned());
        value.floor()
    }
}

impl From<&VirtualPageNumber> for VirtualAddress {
    fn from(value: &VirtualPageNumber) -> Self {
        VirtualAddress(value.0 * config::PAGE_SIZE)
    }
}

impl PhysicalPageNumber {
    pub fn from_ppn(ppn: usize) -> Self {
        PhysicalPageNumber(ppn)
    }

    pub fn get_bytes_array_mut(&self) -> &'static mut [u8] {
        let pa: PhysicalAddress = self.into();
        let vaddr = pa.as_kernel_vaddr();

        // 更严格的检查
        assert!(
            vaddr != 0,
            "get_bytes_array_mut: vaddr为0, ppn={:#x}",
            self.0
        );
        assert!(
            vaddr % config::PAGE_SIZE == 0,
            "get_bytes_array_mut: vaddr未对齐, vaddr={:#x}, ppn={:#x}",
            vaddr,
            self.0
        );

        // 确保vaddr在有效内存范围内
        assert!(
            vaddr >= 0x80000000 && vaddr < 0x88000000,
            "get_bytes_array_mut: vaddr超出有效范围, vaddr={:#x}, ppn={:#x}",
            vaddr,
            self.0
        );

        // 确保大小不会溢出
        assert!(
            config::PAGE_SIZE <= isize::MAX as usize,
            "get_bytes_array_mut: PAGE_SIZE过大, PAGE_SIZE={:#x}",
            config::PAGE_SIZE
        );

        let ptr = vaddr as *mut u8;
        // 验证指针非空和对齐
        assert!(!ptr.is_null(), "get_bytes_array_mut: 指针为空");
        assert!(
            ptr.is_aligned(),
            "get_bytes_array_mut: 指针未对齐, ptr={:p}",
            ptr
        );

        unsafe { core::slice::from_raw_parts_mut(ptr, config::PAGE_SIZE) }
    }

    pub fn get_pte_array(&self) -> &'static mut [PageTableEntry] {
        let pa: PhysicalAddress = self.into();
        unsafe { core::slice::from_raw_parts_mut(pa.as_kernel_vaddr() as *mut PageTableEntry, 512) }
    }

    pub fn get_mut<T>(&self) -> &'static mut T {
        let pa: PhysicalAddress = self.into();
        unsafe { (pa.as_kernel_vaddr() as *mut T).as_mut().unwrap() }
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
}
