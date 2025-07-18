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
    pub pages: usize,  // Number of pages (1 for single page, >1 for contiguous allocation)
}

impl FrameTracker {
    pub fn new(ppn: PhysicalPageNumber) -> Self {
        let bytes_array = ppn.get_bytes_array_mut();
        for byte in bytes_array {
            *byte = 0;
        }
        Self { ppn, pages: 1 }
    }

    pub fn new_contiguous(ppn: PhysicalPageNumber, pages: usize) -> Self {
        // Clear all pages in the contiguous range
        for i in 0..pages {
            let current_ppn = PhysicalPageNumber::from(ppn.as_usize() + i);
            let bytes_array = current_ppn.get_bytes_array_mut();
            for byte in bytes_array {
                *byte = 0;
            }
        }
        Self { ppn, pages }
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        // For contiguous pages, deallocate each page individually
        for i in 0..self.pages {
            let current_ppn = PhysicalPageNumber::from(self.ppn.as_usize() + i);
            let _ = FRAME_ALLOCATOR.wait().lock().dealloc(current_ppn);
        }
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!("FrameTracker PPN:{:#x} pages:{}", self.ppn.as_usize(), self.pages))
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

    pub fn alloc_contiguous(&mut self, pages: usize) -> Option<PhysicalPageNumber> {
        if pages == 0 {
            return None;
        }

        // For contiguous allocation, we can only use the continuous range
        // Cannot use recycled pages as they might not be contiguous
        if self.current_start_ppn.as_usize() + pages <= self.end_ppn.as_usize() {
            let start_ppn = self.current_start_ppn;
            self.current_start_ppn = PhysicalPageNumber::from(self.current_start_ppn.as_usize() + pages);
            Some(start_ppn)
        } else {
            None
        }
    }

    pub fn dealloc(&mut self, ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
        // 验证 PPN 在有效范围内
        if ppn < self.start_ppn || ppn >= self.end_ppn {
            return Err(FrameAllocError::OutOfRange);
        }
        
        // 检查是否试图释放未分配的页面
        // 如果 ppn >= current_start_ppn，说明这个页面还没有被分配过
        if ppn >= self.current_start_ppn {
            return Err(FrameAllocError::OutOfRange);
        }
        
        // 检查重复释放 - 这是一个原子操作在单线程环境下
        if self.recycled_ppns.contains(&ppn) {
            return Err(FrameAllocError::Duplicate);
        }
        
        // 安全地添加到回收列表
        self.recycled_ppns.push(ppn);
        Ok(())
    }
}

pub fn init(start_addr: PhysicalAddress, end_addr: PhysicalAddress) {
    debug!("frame_allocator::init: start_addr={:#x}, end_addr={:#x}", start_addr.as_usize(), end_addr.as_usize());
    
    let start_ppn = start_addr.ceil();
    let end_ppn = end_addr.floor();
    
    debug!("frame_allocator::init: start_ppn={:#x}, end_ppn={:#x}", start_ppn.as_usize(), end_ppn.as_usize());
    
    assert!(
        end_ppn.as_usize() > start_ppn.as_usize(),
        "frame_allocator: range is 0, start_ppn={:#x}, end_ppn={:#x}",
        start_ppn.as_usize(),
        end_ppn.as_usize()
    );
    
    // 验证PPN的合理性
    if start_ppn.as_usize() == 0 {
        panic!("Invalid start PPN: zero page number from address {:#x}", start_addr.as_usize());
    }
    
    FRAME_ALLOCATOR.call_once(|| Mutex::new(StackFrameAllocator::new(start_addr, end_addr)));
}

pub fn alloc() -> Option<FrameTracker> {
    let res = FRAME_ALLOCATOR.wait().lock().alloc();
    res.map(|b| FrameTracker::new(b))
}

pub fn alloc_contiguous(pages: usize) -> Option<FrameTracker> {
    let res = FRAME_ALLOCATOR.wait().lock().alloc_contiguous(pages);
    res.map(|b| FrameTracker::new_contiguous(b, pages))
}

pub fn dealloc(ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
    FRAME_ALLOCATOR.wait().lock().dealloc(ppn)
}
