use core::fmt;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Mutex;
use crate::memory::frame_allocator::FrameTracker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryError {
    AllocationFailed,
    InvalidAddress,
    InvalidAlignment,
    OutOfBounds,
    PermissionDenied,
    NotMapped,
    AlreadyMapped,
    InsufficientMemory,
    FragmentationError,
    CacheError,
    DmaError,
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
            MemoryError::AlreadyMapped => write!(f, "Memory already mapped"),
            MemoryError::InsufficientMemory => write!(f, "Insufficient memory"),
            MemoryError::FragmentationError => write!(f, "Memory fragmentation error"),
            MemoryError::CacheError => write!(f, "Cache operation error"),
            MemoryError::DmaError => write!(f, "DMA operation error"),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAttributes {
    Cached,
    Uncached,
    WriteCombining,
    WriteThrough,
    WriteBack,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheOperation {
    Flush,
    Invalidate,
    Clean,
    FlushAndInvalidate,
}

#[derive(Debug)]
pub struct DmaPool {
    name: alloc::string::String,
    size: usize,
    align: usize,
    allocation_size: usize,
    free_chunks: Mutex<Vec<usize>>,
    used_chunks: Mutex<BTreeMap<usize, usize>>, // address -> size
    base_address: usize,
    coherent: bool,
}

impl DmaPool {
    pub fn new(
        name: alloc::string::String, 
        size: usize, 
        align: usize,
        allocation_size: usize,
        coherent: bool
    ) -> Result<Self, MemoryError> {
        use crate::memory::{frame_allocator::alloc_contiguous, PAGE_SIZE};
        
        let aligned_size = (size + align - 1) & !(align - 1);
        let frames = (aligned_size + PAGE_SIZE - 1) / PAGE_SIZE;
        let frame_tracker = alloc_contiguous(frames).ok_or(MemoryError::AllocationFailed)?;
        let phys_addr = frame_tracker.ppn.as_usize() * PAGE_SIZE;
        
        // For now, use direct mapping. In a real implementation, this would use proper virtual memory
        let base_address = phys_addr;
        
        let num_chunks = aligned_size / allocation_size;
        let mut free_chunks = Vec::new();
        for i in 0..num_chunks {
            free_chunks.push(base_address + i * allocation_size);
        }
        
        Ok(Self {
            name,
            size: aligned_size,
            align,
            allocation_size,
            free_chunks: Mutex::new(free_chunks),
            used_chunks: Mutex::new(BTreeMap::new()),
            base_address,
            coherent,
        })
    }
    
    pub fn allocate(&self, size: usize) -> Result<usize, MemoryError> {
        if size > self.allocation_size {
            return Err(MemoryError::InsufficientMemory);
        }
        
        let mut free_chunks = self.free_chunks.lock();
        let mut used_chunks = self.used_chunks.lock();
        
        if let Some(addr) = free_chunks.pop() {
            used_chunks.insert(addr, size);
            Ok(addr)
        } else {
            Err(MemoryError::InsufficientMemory)
        }
    }
    
    pub fn deallocate(&self, addr: usize) -> Result<(), MemoryError> {
        let mut free_chunks = self.free_chunks.lock();
        let mut used_chunks = self.used_chunks.lock();
        
        if used_chunks.remove(&addr).is_some() {
            free_chunks.push(addr);
            Ok(())
        } else {
            Err(MemoryError::InvalidAddress)
        }
    }
    
    pub fn is_coherent(&self) -> bool {
        self.coherent
    }
    
    pub fn statistics(&self) -> (usize, usize, usize) {
        let free_chunks = self.free_chunks.lock();
        let used_chunks = self.used_chunks.lock();
        
        let free_count = free_chunks.len();
        let used_count = used_chunks.len();
        let total_size = self.size;
        
        (free_count, used_count, total_size)
    }
}

pub trait DmaBuffer: Send + Sync {
    fn physical_address(&self) -> usize;
    fn virtual_address(&self) -> usize;
    fn size(&self) -> usize;
    fn is_coherent(&self) -> bool;
    fn attributes(&self) -> MemoryAttributes;
    
    fn sync_for_cpu(&self) -> Result<(), MemoryError>;
    fn sync_for_device(&self) -> Result<(), MemoryError>;
    fn cache_operation(&self, op: CacheOperation) -> Result<(), MemoryError>;
    
    fn as_slice(&self) -> &[u8];
    fn as_mut_slice(&mut self) -> &mut [u8];
    
    fn clone_buffer(&self) -> Result<Box<dyn DmaBuffer>, MemoryError>;
    fn split_at(&self, offset: usize) -> Result<(Box<dyn DmaBuffer>, Box<dyn DmaBuffer>), MemoryError>;
}

pub struct CoherentDmaBuffer {
    virt_addr: usize,
    phys_addr: usize,
    size: usize,
    attributes: MemoryAttributes,
    _frame_tracker: FrameTracker,
}

pub struct NonCoherentDmaBuffer {
    virt_addr: usize,
    phys_addr: usize,
    size: usize,
    attributes: MemoryAttributes,
    _frame_tracker: FrameTracker,
}

unsafe impl Send for CoherentDmaBuffer {}
unsafe impl Sync for CoherentDmaBuffer {}
unsafe impl Send for NonCoherentDmaBuffer {}
unsafe impl Sync for NonCoherentDmaBuffer {}

impl CoherentDmaBuffer {
    pub fn new(size: usize, attributes: MemoryAttributes) -> Result<Self, MemoryError> {
        use crate::memory::{frame_allocator::alloc_contiguous, PAGE_SIZE};
        
        let frames = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let frame_tracker = alloc_contiguous(frames).ok_or(MemoryError::AllocationFailed)?;
        let phys_addr = frame_tracker.ppn.as_usize() * PAGE_SIZE;
        
        // In a real implementation, we would create a proper virtual mapping
        // For now, we'll use a simple identity mapping for demonstration
        let virt_addr = Self::map_physical_to_virtual(phys_addr, size, attributes)?;
        
        Ok(Self {
            virt_addr,
            phys_addr,
            size,
            attributes,
            _frame_tracker: frame_tracker,
        })
    }
    
    fn map_physical_to_virtual(phys_addr: usize, size: usize, _attributes: MemoryAttributes) -> Result<usize, MemoryError> {
        // This is a simplified implementation. In a real OS, this would:
        // 1. Find free virtual address space
        // 2. Create page table entries with proper attributes
        // 3. Set up cache coherency based on attributes
        
        // For now, we'll use direct mapping with some offset to simulate virtual addressing
        const VIRTUAL_OFFSET: usize = 0x40000000; // Example virtual base
        Ok(phys_addr + VIRTUAL_OFFSET)
    }
    
    fn perform_cache_operation(&self, _op: CacheOperation) -> Result<(), MemoryError> {
        // Coherent DMA doesn't need explicit cache operations
        Ok(())
    }
}

impl NonCoherentDmaBuffer {
    pub fn new(size: usize, attributes: MemoryAttributes) -> Result<Self, MemoryError> {
        use crate::memory::{frame_allocator::alloc_contiguous, PAGE_SIZE};
        
        let frames = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let frame_tracker = alloc_contiguous(frames).ok_or(MemoryError::AllocationFailed)?;
        let phys_addr = frame_tracker.ppn.as_usize() * PAGE_SIZE;
        
        let virt_addr = Self::map_physical_to_virtual(phys_addr, size, attributes)?;
        
        Ok(Self {
            virt_addr,
            phys_addr,
            size,
            attributes,
            _frame_tracker: frame_tracker,
        })
    }
    
    fn map_physical_to_virtual(phys_addr: usize, size: usize, _attributes: MemoryAttributes) -> Result<usize, MemoryError> {
        const VIRTUAL_OFFSET: usize = 0x40000000;
        Ok(phys_addr + VIRTUAL_OFFSET)
    }
    
    fn perform_cache_operation(&self, op: CacheOperation) -> Result<(), MemoryError> {
        let start_addr = self.virt_addr;
        let end_addr = start_addr + self.size;
        
        // Align to cache line boundaries (assuming 64-byte cache lines)
        const CACHE_LINE_SIZE: usize = 64;
        let aligned_start = start_addr & !(CACHE_LINE_SIZE - 1);
        let aligned_end = (end_addr + CACHE_LINE_SIZE - 1) & !(CACHE_LINE_SIZE - 1);
        
        match op {
            CacheOperation::Flush => self.flush_cache_range(aligned_start, aligned_end),
            CacheOperation::Invalidate => self.invalidate_cache_range(aligned_start, aligned_end),
            CacheOperation::Clean => self.clean_cache_range(aligned_start, aligned_end),
            CacheOperation::FlushAndInvalidate => {
                self.flush_cache_range(aligned_start, aligned_end)?;
                self.invalidate_cache_range(aligned_start, aligned_end)
            }
        }
    }
    
    fn flush_cache_range(&self, _start: usize, _end: usize) -> Result<(), MemoryError> {
        unsafe {
            core::arch::asm!("fence");
        }
        Ok(())
    }
    
    fn invalidate_cache_range(&self, _start: usize, _end: usize) -> Result<(), MemoryError> {
        unsafe {
            core::arch::asm!("fence");
        }
        Ok(())
    }
    
    fn clean_cache_range(&self, _start: usize, _end: usize) -> Result<(), MemoryError> {
        unsafe {
            core::arch::asm!("fence");
        }
        Ok(())
    }
}

impl DmaBuffer for CoherentDmaBuffer {
    fn physical_address(&self) -> usize {
        self.phys_addr
    }
    
    fn virtual_address(&self) -> usize {
        self.virt_addr
    }
    
    fn size(&self) -> usize {
        self.size
    }
    
    fn is_coherent(&self) -> bool {
        true
    }
    
    fn attributes(&self) -> MemoryAttributes {
        self.attributes
    }
    
    fn sync_for_cpu(&self) -> Result<(), MemoryError> {
        // Coherent buffers don't need explicit sync
        Ok(())
    }
    
    fn sync_for_device(&self) -> Result<(), MemoryError> {
        // Coherent buffers don't need explicit sync
        Ok(())
    }
    
    fn cache_operation(&self, op: CacheOperation) -> Result<(), MemoryError> {
        self.perform_cache_operation(op)
    }
    
    fn as_slice(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self.virt_addr as *const u8, self.size)
        }
    }
    
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.virt_addr as *mut u8, self.size)
        }
    }
    
    fn clone_buffer(&self) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let new_buffer = CoherentDmaBuffer::new(self.size, self.attributes)?;
        
        // Copy data
        let src = self.as_slice();
        let dst = unsafe {
            core::slice::from_raw_parts_mut(new_buffer.virt_addr as *mut u8, new_buffer.size)
        };
        dst.copy_from_slice(src);
        
        Ok(Box::new(new_buffer))
    }
    
    fn split_at(&self, offset: usize) -> Result<(Box<dyn DmaBuffer>, Box<dyn DmaBuffer>), MemoryError> {
        if offset >= self.size {
            return Err(MemoryError::OutOfBounds);
        }
        
        let first_size = offset;
        let second_size = self.size - offset;
        
        let first_buffer = CoherentDmaBuffer::new(first_size, self.attributes)?;
        let second_buffer = CoherentDmaBuffer::new(second_size, self.attributes)?;
        
        // Copy data
        let src = self.as_slice();
        
        let first_dst = unsafe {
            core::slice::from_raw_parts_mut(first_buffer.virt_addr as *mut u8, first_size)
        };
        first_dst.copy_from_slice(&src[0..first_size]);
        
        let second_dst = unsafe {
            core::slice::from_raw_parts_mut(second_buffer.virt_addr as *mut u8, second_size)
        };
        second_dst.copy_from_slice(&src[first_size..]);
        
        Ok((Box::new(first_buffer), Box::new(second_buffer)))
    }
}

impl DmaBuffer for NonCoherentDmaBuffer {
    fn physical_address(&self) -> usize {
        self.phys_addr
    }
    
    fn virtual_address(&self) -> usize {
        self.virt_addr
    }
    
    fn size(&self) -> usize {
        self.size
    }
    
    fn is_coherent(&self) -> bool {
        false
    }
    
    fn attributes(&self) -> MemoryAttributes {
        self.attributes
    }
    
    fn sync_for_cpu(&self) -> Result<(), MemoryError> {
        self.perform_cache_operation(CacheOperation::Invalidate)
    }
    
    fn sync_for_device(&self) -> Result<(), MemoryError> {
        self.perform_cache_operation(CacheOperation::Clean)
    }
    
    fn cache_operation(&self, op: CacheOperation) -> Result<(), MemoryError> {
        self.perform_cache_operation(op)
    }
    
    fn as_slice(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self.virt_addr as *const u8, self.size)
        }
    }
    
    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.virt_addr as *mut u8, self.size)
        }
    }
    
    fn clone_buffer(&self) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let new_buffer = NonCoherentDmaBuffer::new(self.size, self.attributes)?;
        
        // Copy data
        let src = self.as_slice();
        let dst = unsafe {
            core::slice::from_raw_parts_mut(new_buffer.virt_addr as *mut u8, new_buffer.size)
        };
        dst.copy_from_slice(src);
        
        Ok(Box::new(new_buffer))
    }
    
    fn split_at(&self, offset: usize) -> Result<(Box<dyn DmaBuffer>, Box<dyn DmaBuffer>), MemoryError> {
        if offset >= self.size {
            return Err(MemoryError::OutOfBounds);
        }
        
        let first_size = offset;
        let second_size = self.size - offset;
        
        let first_buffer = NonCoherentDmaBuffer::new(first_size, self.attributes)?;
        let second_buffer = NonCoherentDmaBuffer::new(second_size, self.attributes)?;
        
        // Copy data
        let src = self.as_slice();
        
        let first_dst = unsafe {
            core::slice::from_raw_parts_mut(first_buffer.virt_addr as *mut u8, first_size)
        };
        first_dst.copy_from_slice(&src[0..first_size]);
        
        let second_dst = unsafe {
            core::slice::from_raw_parts_mut(second_buffer.virt_addr as *mut u8, second_size)
        };
        second_dst.copy_from_slice(&src[first_size..]);
        
        Ok((Box::new(first_buffer), Box::new(second_buffer)))
    }
}


pub trait DmaManager: Send + Sync {
    fn allocate_buffer(&self, size: usize, coherent: bool) -> Result<Box<dyn DmaBuffer>, MemoryError>;
    fn allocate_buffer_with_attributes(&self, size: usize, coherent: bool, attributes: MemoryAttributes) -> Result<Box<dyn DmaBuffer>, MemoryError>;
    fn map_memory(&self, phys_addr: usize, size: usize, permission: MemoryPermission) -> Result<usize, MemoryError>;
    fn unmap_memory(&self, virt_addr: usize, size: usize) -> Result<(), MemoryError>;
    fn create_pool(&self, name: alloc::string::String, size: usize, align: usize, allocation_size: usize, coherent: bool) -> Result<Arc<DmaPool>, MemoryError>;
    fn get_pool(&self, name: &str) -> Option<Arc<DmaPool>>;
    fn remove_pool(&self, name: &str) -> Result<(), MemoryError>;
}

pub struct CoherentDmaManager {
    pools: Mutex<BTreeMap<alloc::string::String, Arc<DmaPool>>>,
    mappings: Mutex<BTreeMap<usize, MemoryMapping>>, // virt_addr -> mapping
    next_virt_addr: AtomicUsize,
}

pub struct NonCoherentDmaManager {
    pools: Mutex<BTreeMap<alloc::string::String, Arc<DmaPool>>>,
    mappings: Mutex<BTreeMap<usize, MemoryMapping>>,
    next_virt_addr: AtomicUsize,
}

impl CoherentDmaManager {
    pub fn new() -> Self {
        const COHERENT_DMA_BASE: usize = 0x50000000;
        Self {
            pools: Mutex::new(BTreeMap::new()),
            mappings: Mutex::new(BTreeMap::new()),
            next_virt_addr: AtomicUsize::new(COHERENT_DMA_BASE),
        }
    }
    
    fn allocate_virtual_address(&self, size: usize, align: usize) -> Result<usize, MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let aligned_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        
        loop {
            let current = self.next_virt_addr.load(Ordering::Relaxed);
            let aligned_addr = (current + align - 1) & !(align - 1);
            let new_addr = aligned_addr + aligned_size;
            
            if self.next_virt_addr.compare_exchange_weak(
                current, 
                new_addr, 
                Ordering::Relaxed, 
                Ordering::Relaxed
            ).is_ok() {
                return Ok(aligned_addr);
            }
        }
    }
}

impl DmaManager for CoherentDmaManager {
    fn allocate_buffer(&self, size: usize, _coherent: bool) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let buffer = CoherentDmaBuffer::new(size, MemoryAttributes::Cached)?;
        Ok(Box::new(buffer))
    }
    
    fn allocate_buffer_with_attributes(&self, size: usize, _coherent: bool, attributes: MemoryAttributes) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let buffer = CoherentDmaBuffer::new(size, attributes)?;
        Ok(Box::new(buffer))
    }
    
    fn map_memory(&self, phys_addr: usize, size: usize, permission: MemoryPermission) -> Result<usize, MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let virt_addr = self.allocate_virtual_address(size, PAGE_SIZE)?;
        
        let mapping = MemoryMapping::new(virt_addr, phys_addr, size, permission);
        let mut mappings = self.mappings.lock();
        mappings.insert(virt_addr, mapping);
        
        Ok(virt_addr)
    }
    
    fn unmap_memory(&self, virt_addr: usize, _size: usize) -> Result<(), MemoryError> {
        let mut mappings = self.mappings.lock();
        if mappings.remove(&virt_addr).is_some() {
            Ok(())
        } else {
            Err(MemoryError::NotMapped)
        }
    }
    
    fn create_pool(&self, name: alloc::string::String, size: usize, align: usize, allocation_size: usize, coherent: bool) -> Result<Arc<DmaPool>, MemoryError> {
        let pool = Arc::new(DmaPool::new(name.clone(), size, align, allocation_size, coherent)?);
        
        let mut pools = self.pools.lock();
        pools.insert(name, pool.clone());
        
        Ok(pool)
    }
    
    fn get_pool(&self, name: &str) -> Option<Arc<DmaPool>> {
        let pools = self.pools.lock();
        pools.get(name).cloned()
    }
    
    fn remove_pool(&self, name: &str) -> Result<(), MemoryError> {
        let mut pools = self.pools.lock();
        if pools.remove(name).is_some() {
            Ok(())
        } else {
            Err(MemoryError::InvalidAddress)
        }
    }
}

impl NonCoherentDmaManager {
    pub fn new() -> Self {
        const NON_COHERENT_DMA_BASE: usize = 0x60000000;
        Self {
            pools: Mutex::new(BTreeMap::new()),
            mappings: Mutex::new(BTreeMap::new()),
            next_virt_addr: AtomicUsize::new(NON_COHERENT_DMA_BASE),
        }
    }
    
    fn allocate_virtual_address(&self, size: usize, align: usize) -> Result<usize, MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let aligned_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        
        loop {
            let current = self.next_virt_addr.load(Ordering::Relaxed);
            let aligned_addr = (current + align - 1) & !(align - 1);
            let new_addr = aligned_addr + aligned_size;
            
            if self.next_virt_addr.compare_exchange_weak(
                current, 
                new_addr, 
                Ordering::Relaxed, 
                Ordering::Relaxed
            ).is_ok() {
                return Ok(aligned_addr);
            }
        }
    }
}

impl DmaManager for NonCoherentDmaManager {
    fn allocate_buffer(&self, size: usize, _coherent: bool) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let buffer = NonCoherentDmaBuffer::new(size, MemoryAttributes::Uncached)?;
        Ok(Box::new(buffer))
    }
    
    fn allocate_buffer_with_attributes(&self, size: usize, _coherent: bool, attributes: MemoryAttributes) -> Result<Box<dyn DmaBuffer>, MemoryError> {
        let buffer = NonCoherentDmaBuffer::new(size, attributes)?;
        Ok(Box::new(buffer))
    }
    
    fn map_memory(&self, phys_addr: usize, size: usize, permission: MemoryPermission) -> Result<usize, MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let virt_addr = self.allocate_virtual_address(size, PAGE_SIZE)?;
        
        let mapping = MemoryMapping::new(virt_addr, phys_addr, size, permission);
        let mut mappings = self.mappings.lock();
        mappings.insert(virt_addr, mapping);
        
        Ok(virt_addr)
    }
    
    fn unmap_memory(&self, virt_addr: usize, _size: usize) -> Result<(), MemoryError> {
        let mut mappings = self.mappings.lock();
        if mappings.remove(&virt_addr).is_some() {
            Ok(())
        } else {
            Err(MemoryError::NotMapped)
        }
    }
    
    fn create_pool(&self, name: alloc::string::String, size: usize, align: usize, allocation_size: usize, coherent: bool) -> Result<Arc<DmaPool>, MemoryError> {
        let pool = Arc::new(DmaPool::new(name.clone(), size, align, allocation_size, coherent)?);
        
        let mut pools = self.pools.lock();
        pools.insert(name, pool.clone());
        
        Ok(pool)
    }
    
    fn get_pool(&self, name: &str) -> Option<Arc<DmaPool>> {
        let pools = self.pools.lock();
        pools.get(name).cloned()
    }
    
    fn remove_pool(&self, name: &str) -> Result<(), MemoryError> {
        let mut pools = self.pools.lock();
        if pools.remove(name).is_some() {
            Ok(())
        } else {
            Err(MemoryError::InvalidAddress)
        }
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

pub struct IoRemap {
    mappings: Mutex<BTreeMap<usize, MemoryMapping>>, // phys_addr -> mapping
    next_virt_addr: AtomicUsize,
}

impl IoRemap {
    pub fn new() -> Self {
        const IO_REMAP_BASE: usize = 0x70000000;
        Self {
            mappings: Mutex::new(BTreeMap::new()),
            next_virt_addr: AtomicUsize::new(IO_REMAP_BASE),
        }
    }
    
    pub fn ioremap(&self, phys_addr: usize, size: usize, attributes: MemoryAttributes) -> Result<usize, MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let aligned_phys = phys_addr & !(PAGE_SIZE - 1);
        let offset = phys_addr - aligned_phys;
        let aligned_size = ((size + offset + PAGE_SIZE - 1) / PAGE_SIZE) * PAGE_SIZE;
        
        // Check if already mapped
        {
            let mappings = self.mappings.lock();
            for mapping in mappings.values() {
                if mapping.physical_address() == aligned_phys && mapping.size() >= aligned_size {
                    return Ok(mapping.virtual_address() + offset);
                }
            }
        }
        
        // Allocate new virtual address
        let virt_addr = self.allocate_virtual_address(aligned_size)?;
        
        // Create mapping (in a real implementation, this would update page tables)
        let mapping = MemoryMapping::new(
            virt_addr,
            aligned_phys,
            aligned_size,
            MemoryPermission::ReadWrite, // IO mappings are typically read-write
        );
        
        let mut mappings = self.mappings.lock();
        mappings.insert(aligned_phys, mapping);
        
        Ok(virt_addr + offset)
    }
    
    pub fn iounmap(&self, virt_addr: usize) -> Result<(), MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let aligned_virt = virt_addr & !(PAGE_SIZE - 1);
        
        let mut mappings = self.mappings.lock();
        let mapping_key = mappings.iter()
            .find(|(_, mapping)| mapping.virtual_address() == aligned_virt)
            .map(|(&key, _)| key);
        
        if let Some(key) = mapping_key {
            mappings.remove(&key);
            Ok(())
        } else {
            Err(MemoryError::NotMapped)
        }
    }
    
    fn allocate_virtual_address(&self, size: usize) -> Result<usize, MemoryError> {
        use crate::memory::PAGE_SIZE;
        
        let aligned_size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        
        loop {
            let current = self.next_virt_addr.load(Ordering::Relaxed);
            let new_addr = current + aligned_size;
            
            if self.next_virt_addr.compare_exchange_weak(
                current, 
                new_addr, 
                Ordering::Relaxed, 
                Ordering::Relaxed
            ).is_ok() {
                return Ok(current);
            }
        }
    }
    
    pub fn get_physical_address(&self, virt_addr: usize) -> Option<usize> {
        use crate::memory::PAGE_SIZE;
        
        let aligned_virt = virt_addr & !(PAGE_SIZE - 1);
        let offset = virt_addr - aligned_virt;
        
        let mappings = self.mappings.lock();
        for mapping in mappings.values() {
            if mapping.contains(aligned_virt) {
                return Some(mapping.physical_address() + offset);
            }
        }
        
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HugePageSize {
    Size2MB = 21,   // 2^21 bytes
    Size1GB = 30,   // 2^30 bytes
}

impl HugePageSize {
    pub fn size(&self) -> usize {
        1 << (*self as usize)
    }
    
    pub fn alignment(&self) -> usize {
        self.size()
    }
}

pub struct HugePageAllocator {
    pools: Mutex<BTreeMap<HugePageSize, Vec<usize>>>, // size -> list of free pages
    allocated: Mutex<BTreeMap<usize, HugePageSize>>,  // address -> size
    base_address: usize,
    total_size: usize,
}

impl HugePageAllocator {
    pub fn new(base_address: usize, total_size: usize) -> Self {
        let mut pools = BTreeMap::new();
        pools.insert(HugePageSize::Size2MB, Vec::new());
        pools.insert(HugePageSize::Size1GB, Vec::new());
        
        // Initialize with available huge pages
        let allocator = Self {
            pools: Mutex::new(pools),
            allocated: Mutex::new(BTreeMap::new()),
            base_address,
            total_size,
        };
        
        allocator.initialize_pools();
        allocator
    }
    
    fn initialize_pools(&self) {
        let mut pools = self.pools.lock();
        
        // Start with 1GB pages where possible
        let gb_size = HugePageSize::Size1GB.size();
        let num_gb_pages = self.total_size / gb_size;
        
        if let Some(gb_pool) = pools.get_mut(&HugePageSize::Size1GB) {
            for i in 0..num_gb_pages {
                gb_pool.push(self.base_address + i * gb_size);
            }
        }
        
        // Use remainder for 2MB pages
        let remaining_start = self.base_address + num_gb_pages * gb_size;
        let remaining_size = self.total_size - num_gb_pages * gb_size;
        let mb_size = HugePageSize::Size2MB.size();
        let num_mb_pages = remaining_size / mb_size;
        
        if let Some(mb_pool) = pools.get_mut(&HugePageSize::Size2MB) {
            for i in 0..num_mb_pages {
                mb_pool.push(remaining_start + i * mb_size);
            }
        }
    }
    
    pub fn allocate(&self, size: usize, huge_page_size: HugePageSize) -> Result<usize, MemoryError> {
        if size > huge_page_size.size() {
            return Err(MemoryError::InsufficientMemory);
        }
        
        let mut pools = self.pools.lock();
        let mut allocated = self.allocated.lock();
        
        if let Some(pool) = pools.get_mut(&huge_page_size) {
            if let Some(addr) = pool.pop() {
                allocated.insert(addr, huge_page_size);
                return Ok(addr);
            }
        }
        
        // Try to split a larger page if available
        if huge_page_size == HugePageSize::Size2MB {
            if let Some(gb_pool) = pools.get_mut(&HugePageSize::Size1GB) {
                if let Some(gb_addr) = gb_pool.pop() {
                    // Split 1GB page into 512 2MB pages
                    let mb_size = HugePageSize::Size2MB.size();
                    let mb_pool = pools.get_mut(&HugePageSize::Size2MB).unwrap();
                    
                    // Keep first 2MB page for the allocation
                    allocated.insert(gb_addr, HugePageSize::Size2MB);
                    
                    // Add remaining 511 2MB pages to the pool
                    for i in 1..512 {
                        mb_pool.push(gb_addr + i * mb_size);
                    }
                    
                    return Ok(gb_addr);
                }
            }
        }
        
        Err(MemoryError::InsufficientMemory)
    }
    
    pub fn deallocate(&self, addr: usize) -> Result<(), MemoryError> {
        let mut pools = self.pools.lock();
        let mut allocated = self.allocated.lock();
        
        if let Some(size) = allocated.remove(&addr) {
            if let Some(pool) = pools.get_mut(&size) {
                pool.push(addr);
                Ok(())
            } else {
                Err(MemoryError::InvalidAddress)
            }
        } else {
            Err(MemoryError::InvalidAddress)
        }
    }
    
    pub fn statistics(&self) -> BTreeMap<HugePageSize, (usize, usize)> {
        let pools = self.pools.lock();
        let allocated = self.allocated.lock();
        
        let mut stats = BTreeMap::new();
        
        for (&size, pool) in pools.iter() {
            let free_count = pool.len();
            let allocated_count = allocated.values().filter(|&&s| s == size).count();
            stats.insert(size, (free_count, allocated_count));
        }
        
        stats
    }
    
    pub fn defragment(&self) -> Result<usize, MemoryError> {
        let mut pools = self.pools.lock();
        let mut merged_count = 0;
        
        let mut merged_pages = Vec::new();
        let mut added_gb_pages = Vec::new();
        let mb_size = HugePageSize::Size2MB.size();
        let gb_size = HugePageSize::Size1GB.size();
        let pages_per_gb = gb_size / mb_size; // 512
        
        // Try to merge 512 consecutive 2MB pages into 1GB pages
        if let Some(mb_pool) = pools.get_mut(&HugePageSize::Size2MB) {
            mb_pool.sort_unstable();
            
            let mut i = 0;
            
            while i + pages_per_gb <= mb_pool.len() {
                let base_addr = mb_pool[i];
                let mut consecutive = true;
                
                // Check if we have 512 consecutive 2MB pages
                for j in 1..pages_per_gb {
                    if mb_pool[i + j] != base_addr + j * mb_size {
                        consecutive = false;
                        break;
                    }
                }
                
                if consecutive && (base_addr % gb_size == 0) {
                    // We can merge these pages
                    merged_pages.extend(i..i+pages_per_gb);
                    merged_count += 1;
                    i += pages_per_gb;
                } else {
                    i += 1;
                }
            }
            
            // Remove merged pages from 2MB pool (in reverse order to maintain indices)  
            for &idx in merged_pages.iter().rev() {
                mb_pool.remove(idx);
            }
            
            // Calculate the GB page addresses to add
            for chunk in merged_pages.chunks(pages_per_gb) {
                if chunk.len() == pages_per_gb {
                    // This represents a full GB worth of 2MB pages
                    if let Some(&first_idx) = chunk.first() {
                        if first_idx < mb_pool.len() + chunk.len() {
                            // Calculate base address of the GB page that was formed
                            let base_addr = (first_idx / pages_per_gb) * gb_size;
                            added_gb_pages.push(base_addr);
                        }
                    }
                }
            }
        }
        
        // Now add the merged pages to GB pool (after releasing mb_pool borrow)
        if !merged_pages.is_empty() {
            if let Some(gb_pool) = pools.get_mut(&HugePageSize::Size1GB) {
                for addr in added_gb_pages {
                    gb_pool.push(addr);
                }
            }
        }
        
        Ok(merged_count)
    }
}