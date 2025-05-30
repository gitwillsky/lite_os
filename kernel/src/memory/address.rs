use super::config;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct PhysicalAddress(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct VirtualAddress(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct PhysicalPageNumber(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct VirtualPageNumber(usize);

impl From<usize> for PhysicalAddress {
    fn from(addr: usize) -> Self {
        PhysicalAddress(addr)
    }
}

impl From<PhysicalAddress> for usize {
    fn from(addr: PhysicalAddress) -> Self {
        addr.0
    }
}

impl From<usize> for VirtualAddress {
    fn from(addr: usize) -> Self {
        VirtualAddress(addr)
    }
}

impl From<VirtualAddress> for usize {
    fn from(addr: VirtualAddress) -> Self {
        addr.0
    }
}

impl From<usize> for PhysicalPageNumber {
    fn from(addr: usize) -> Self {
        PhysicalPageNumber(addr)
    }
}

impl From<usize> for VirtualPageNumber {
    fn from(addr: usize) -> Self {
        VirtualPageNumber(addr)
    }
}

impl PhysicalAddress {
    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn page_number(&self) -> PhysicalPageNumber {
        PhysicalPageNumber(self.0 / config::PAGE_SIZE)
    }

    pub fn page_offset(&self) -> usize {
        self.0 & (config::PAGE_SIZE - 1)
    }

    pub fn floor(&self) -> PhysicalPageNumber {
        self.page_number()
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

    pub fn as_usize(&self) -> usize {
        self.0
    }
}

impl From<PhysicalPageNumber> for PhysicalAddress {
    fn from(ppn: PhysicalPageNumber) -> Self {
        PhysicalAddress(ppn.0 * config::PAGE_SIZE)
    }
}

impl PhysicalPageNumber {
    pub fn as_usize(&self) -> usize {
        self.0
    }

    pub fn get_bytes_mut(&self) -> &'static mut [u8; config::PAGE_SIZE] {
        unsafe {
            ((self.0 * config::PAGE_SIZE) as *mut [u8; config::PAGE_SIZE])
                .as_mut()
                .unwrap()
        }
    }
}
