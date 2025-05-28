use super::address::{PhysicalAddress, PhysicalPageNumber};
use alloc::vec::Vec;
use spin::{Mutex, Once};

static FRAME_ALLOCATOR: Once<Mutex<StackFrameAllocator>> = Once::new();

#[derive(Debug)]
pub enum FrameAllocError {
    OutOfRange,
    Duplicate,
}

#[derive(Debug)]
pub struct StackFrameAllocator {
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
        if !self.recycled_ppns.is_empty() {
            return self.recycled_ppns.pop();
        }
        if self.current_start_ppn < self.end_ppn {
            let current = self.current_start_ppn;
            self.current_start_ppn = (current.as_usize() + 1).into();
            return Some(current);
        }
        return None;
    }

    pub fn dealloc(&mut self, ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
        if ppn < self.start_ppn || ppn > self.end_ppn {
            return Err(FrameAllocError::OutOfRange);
        }
        if ppn > self.current_start_ppn && ppn < self.end_ppn {
            return Err(FrameAllocError::OutOfRange);
        }
        if self.recycled_ppns.contains(&ppn) {
            return Err(FrameAllocError::Duplicate);
        }
        Ok(self.recycled_ppns.push(ppn))
    }
}

pub fn init(start_addr: usize, end_addr: usize) {
    FRAME_ALLOCATOR
        .call_once(|| Mutex::new(StackFrameAllocator::new(start_addr.into(), end_addr.into())));
}

impl Drop for PhysicalPageNumber {
    fn drop(&mut self) {
        FRAME_ALLOCATOR.wait().lock().dealloc(*self);
    }
}

pub fn alloc() -> Option<PhysicalPageNumber> {
    FRAME_ALLOCATOR.wait().lock().alloc().map(|b| {
        for i in b.get_bytes_mut() {
            *i = 0;
        }
        b
    })
}

pub fn dealloc(ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
    FRAME_ALLOCATOR.wait().lock().dealloc(ppn)
}
