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

impl From<usize> for VirtualAddress {
    fn from(addr: usize) -> Self {
        VirtualAddress(addr)
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
