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
        // 保留传入地址的规范形式（canonical address）。
        // 不再截断为低 39 位，防止高半区地址（如 TRAMPOLINE）被错误折叠到低半区。
        VirtualAddress(addr)
    }
}

impl From<PhysicalAddress> for usize {
    fn from(addr: PhysicalAddress) -> Self {
        addr.0
    }
}

impl From<VirtualAddress> for usize {
    fn from(addr: VirtualAddress) -> Self {
        // 对 Sv39 虚拟地址做正确的符号扩展：
        // 若 bit[38] 为 1，则高位应填充为 1；否则填充为 0。
        let mask: usize = (1usize << config::VIRTUAL_ADDRESS_WIDTH) - 1; // 低 39 位掩码
        let sign_bit: usize = 1usize << (config::VIRTUAL_ADDRESS_WIDTH - 1); // 第 38 位
        let raw: usize = addr.0 & mask;
        if (raw & sign_bit) != 0 {
            raw | (!mask)
        } else {
            raw
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

    /// @description 将物理地址表示为只读裸指针，不创建引用或声明别名关系。
    ///
    /// @return 指向恒等映射物理地址的裸指针；调用方在解引用前必须证明映射、对齐和生命周期有效。
    pub(crate) fn as_ptr<T>(&self) -> *const T {
        self.0 as *const T
    }

    /// @description 将物理地址表示为可写裸指针，不创建引用或声明独占访问。
    ///
    /// @return 指向恒等映射物理地址的裸指针；调用方在解引用前必须证明映射、对齐、生命周期和独占访问有效。
    pub(crate) fn as_mut_ptr<T>(&self) -> *mut T {
        self.0 as *mut T
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
        let addr = ppn.0 * config::PAGE_SIZE;
        PhysicalAddress(addr)
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
    /// @description 返回物理页起始位置的只读裸指针，不创建引用。
    ///
    /// @return 指向该物理页第一个字节的裸指针；调用方负责证明页帧仍存活。
    pub(crate) fn as_page_ptr(self) -> *const u8 {
        let pa: PhysicalAddress = self.into();
        pa.as_ptr()
    }

    /// @description 返回物理页起始位置的可写裸指针，不创建引用。
    ///
    /// @return 指向该物理页第一个字节的裸指针；调用方负责证明页帧存活且当前访问独占。
    pub(crate) fn as_page_mut_ptr(self) -> *mut u8 {
        let pa: PhysicalAddress = self.into();
        pa.as_mut_ptr()
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
