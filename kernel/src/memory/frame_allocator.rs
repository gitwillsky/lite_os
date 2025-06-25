use core::fmt::Debug;

use super::address::{PhysicalAddress, PhysicalPageNumber};
use alloc::vec::Vec;
use spin::{Mutex, Once};

static FRAME_ALLOCATOR: Once<Mutex<StackFrameAllocator>> = Once::new();

#[derive(Debug)]
pub enum FrameAllocError {
    OutOfRange,
    Duplicate,
}

pub struct FrameTracker {
    pub ppn: PhysicalPageNumber,
}

impl FrameTracker {
    pub fn new(ppn: PhysicalPageNumber) -> Self {
        let vaddr = ppn.get_bytes_array_mut().as_ptr() as *mut u8;
        unsafe {
            core::ptr::write_bytes(vaddr, 0, crate::memory::config::PAGE_SIZE);
        }
        Self { ppn }
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        let _ = FRAME_ALLOCATOR.wait().lock().dealloc(self.ppn);
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("FrameTracker PPN:{:#x}", self.ppn.as_usize()))
    }
}

#[derive(Debug)]
struct StackFrameAllocator {
    start_ppn: PhysicalPageNumber,
    current_start_ppn: PhysicalPageNumber,
    end_ppn: PhysicalPageNumber,

    recycled_ppns: Vec<PhysicalPageNumber>,
}

impl StackFrameAllocator {
    pub fn new(start_addr: PhysicalAddress, end_addr: PhysicalAddress) -> Self {
        let start = start_addr.ceil();
        Self {
            start_ppn: start,
            current_start_ppn: start,
            end_ppn: end_addr.floor(),
            recycled_ppns: Vec::new(),
        }
    }

    pub fn alloc(&mut self) -> Option<PhysicalPageNumber> {
        if let Some(ppn) = self.recycled_ppns.pop() {
            Some(ppn)
        } else if self.current_start_ppn < self.end_ppn {
            let current = self.current_start_ppn;
            self.current_start_ppn = current.add_one();
            Some(current)
        } else {
            None
        }
    }

    pub fn dealloc(&mut self, ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
        assert!(
            ppn >= self.start_ppn && ppn < self.end_ppn,
            "dealloc: 非法ppn={:#x}, 合法区间=[{:#x}, {:#x})",
            ppn.as_usize(),
            self.start_ppn.as_usize(),
            self.end_ppn.as_usize()
        );
        if ppn > self.current_start_ppn && ppn < self.end_ppn {
            return Err(FrameAllocError::OutOfRange);
        }
        if self.recycled_ppns.contains(&ppn) {
            return Err(FrameAllocError::Duplicate);
        }
        Ok(self.recycled_ppns.push(ppn))
    }
}

pub fn init(start_addr: PhysicalAddress, end_addr: PhysicalAddress) {
    let start_ppn = start_addr.ceil();
    let end_ppn = end_addr.floor();
    assert!(
        end_ppn.as_usize() > start_ppn.as_usize(),
        "frame_allocator: 分配区间为0，start_ppn={:#x}, end_ppn={:#x}",
        start_ppn.as_usize(),
        end_ppn.as_usize()
    );
    FRAME_ALLOCATOR.call_once(|| Mutex::new(StackFrameAllocator::new(start_addr, end_addr)));
}

pub fn alloc() -> Option<FrameTracker> {
    let res = FRAME_ALLOCATOR.wait().lock().alloc();
    res.map(|b| FrameTracker::new(b))
}

pub fn dealloc(ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
    FRAME_ALLOCATOR.wait().lock().dealloc(ppn)
}
