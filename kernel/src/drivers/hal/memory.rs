use core::fmt;
use alloc::vec::Vec;
use alloc::boxed::Box;
use core::ptr::NonNull;
use crate::memory::frame_allocator::FrameTracker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    AllocationFailed,
    InvalidAddress,
    InvalidAlignment,
    OutOfBounds,
    PermissionDenied,
    NotMapped,
}

impl fmt::Display for MemoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MemoryError::AllocationFailed => write!(f, "Memory allocation failed"),
            MemoryError::InvalidAddress => write!(f, "Invalid memory address"),
            MemoryError::InvalidAlignment => write!(f, "Invalid memory alignment"),
            MemoryError::OutOfBounds => write!(f, "Memory access out of bounds"),
            MemoryError::PermissionDenied => write!(f, "Memory access permission denied"),
            MemoryError::NotMapped => write!(f, "Memory not mapped"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPermission {
    Read,
    Write,
    Execute,
    ReadWrite,
    ReadExecute,
    WriteExecute,
    ReadWriteExecute,
}

pub trait DmaBuffer: Send + Sync {
    fn physical_address(&self) -> usize;
    fn virtual_address(&self) -> usize;
    fn size(&self) -> usize;
    fn is_coherent(&self) -> bool;
    
    fn sync_for_cpu(&self) -> Result<(), MemoryError>;
    fn sync_for_device(&self) -> Result<(), MemoryError>;
    
    fn as_slice(&self) -> &[u8];
    fn as_mut_slice(&mut self) -> &mut [u8];
}

pub struct SimpleDmaBuffer {
    virt_addr: NonNull<u8>,
    phys_addr: usize,
    size: usize,
    coherent: bool,
    _frame_tracker: FrameTracker,
}

unsafe impl Send for SimpleDmaBuffer {}
unsafe impl Sync for SimpleDmaBuffer {}

impl SimpleDmaBuffer {
    pub fn new(size: usize, coherent: bool) -> Result<Self, MemoryError> {
        use crate::memory::{frame_allocator::alloc_contiguous, PAGE_SIZE};
        
        let frames = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let frame_tracker = alloc_contiguous(frames).ok_or(MemoryError::AllocationFailed)?;
        let phys_addr = frame_tracker.ppn.as_usize() * PAGE_SIZE;
        
        let virt_addr = NonNull::new(phys_addr as *mut u8)
            .ok_or(MemoryError::InvalidAddress)?;
        
        Ok(Self {
            virt_addr,
            phys_addr,
            size,
            coherent,
            _frame_tracker: frame_tracker,
        })
    }
}

impl DmaBuffer for SimpleDmaBuffer {
    fn physical_address(&self) -> usize {
        self.phys_addr
    }
    
    fn virtual_address(&self) -> usize {
        self.virt_addr.as_ptr() as usize
    }
    
    fn size(&self) -> usize {
        self.size
    }
    
    fn is_coherent(&self) -> bool {
        self.coherent
    }
    
    fn sync_for_cpu(&self) -> Result<(), MemoryError> {
        if !self.coherent {
            unsafe {
                core::arch::asm!("fence");
            }
        }
        Ok(())
    }
    
    fn sync_for_device(&self) -> Result<(), MemoryError> {
        if !self.coherent {
            unsafe {
                core::arch::asm!("fence");
            }
        }
        Ok(())
    }
    
    fn as_slice(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self.virt_addr.as_ptr(), self.size)
        }
    }
    
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.virt_addr.as_ptr(), self.size)
        }
    }
}


pub trait DmaManager: Send + Sync {
    fn allocate_buffer(&self, size: usize, coherent: bool) -> Result<Box<dyn DmaBuffer>, MemoryError>;
    fn map_memory(&self, phys_addr: usize, size: usize, permission: MemoryPermission) -> Result<usize, MemoryError>;
    fn unmap_memory(&self, virt_addr: usize, size: usize) -> Result<(), MemoryError>;
}

pub struct SimpleDmaManager;

impl SimpleDmaManager {
    pub fn new() -> Self {
        Self
    }
}

impl DmaManager for SimpleDmaManager {
    fn allocate_buffer(&self, size: usize, coherent: bool) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let buffer = SimpleDmaBuffer::new(size, coherent)?;
        Ok(Box::new(buffer))
    }
    
    fn map_memory(&self, phys_addr: usize, size: usize, _permission: MemoryPermission) -> Result<usize, MemoryError> {
        Ok(phys_addr)
    }
    
    fn unmap_memory(&self, _virt_addr: usize, _size: usize) -> Result<(), MemoryError> {
        Ok(())
    }
}

pub struct MemoryMapping {
    virt_addr: usize,
    phys_addr: usize,
    size: usize,
    permission: MemoryPermission,
}

impl MemoryMapping {
    pub fn new(
        virt_addr: usize,
        phys_addr: usize,
        size: usize,
        permission: MemoryPermission,
    ) -> Self {
        Self {
            virt_addr,
            phys_addr,
            size,
            permission,
        }
    }
    
    pub fn virtual_address(&self) -> usize {
        self.virt_addr
    }
    
    pub fn physical_address(&self) -> usize {
        self.phys_addr
    }
    
    pub fn size(&self) -> usize {
        self.size
    }
    
    pub fn permission(&self) -> MemoryPermission {
        self.permission
    }
    
    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.virt_addr && addr < self.virt_addr + self.size
    }
}