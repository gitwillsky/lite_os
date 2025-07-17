use core::alloc::Layout;
use core::mem::size_of;
use core::ptr::NonNull;
use spin::Mutex;

use crate::memory::frame_allocator::{alloc as frame_alloc, FrameTracker};
use crate::memory::config::PAGE_SIZE;

/// SLAB allocator error types
#[derive(Debug, Clone, Copy)]
pub enum SlabError {
    OutOfMemory,
    InvalidLayout,
    InvalidPointer,
}

/// Free object list node - 使用更安全的方式
#[repr(C)]
struct FreeNode {
    next: Option<usize>, // 使用索引而不是原始指针
}

unsafe impl Send for FreeNode {}
unsafe impl Sync for FreeNode {}

/// SLAB structure containing objects of the same size - 使用链表设计
pub struct Slab {
    /// Start address of the slab
    start: NonNull<u8>,
    /// Size of each object in bytes
    object_size: usize,
    /// Number of objects in this slab
    object_count: usize,
    /// Head of free objects list (索引)
    free_head: Option<usize>,
    /// Number of free objects
    free_count: usize,
    /// Frame tracker for memory management
    _frame: FrameTracker,
    /// Frame tracker for the slab structure itself
    _slab_frame: Option<FrameTracker>,
    /// Next slab in the cache list (using raw pointer to avoid Vec)
    next: Option<NonNull<Slab>>,
}

impl Slab {
    /// 获取对象的索引
    fn get_object_index(&self, ptr: NonNull<u8>) -> Result<usize, SlabError> {
        let addr = ptr.as_ptr() as usize;
        let start_addr = self.start.as_ptr() as usize;
        let end_addr = start_addr + PAGE_SIZE;

        if addr < start_addr || addr >= end_addr {
            return Err(SlabError::InvalidPointer);
        }

        let offset = addr - start_addr;
        if offset % self.object_size != 0 {
            return Err(SlabError::InvalidPointer);
        }

        Ok(offset / self.object_size)
    }

    /// 通过索引获取对象指针
    fn get_object_ptr(&self, index: usize) -> Result<NonNull<u8>, SlabError> {
        if index >= self.object_count {
            return Err(SlabError::InvalidPointer);
        }

        unsafe {
            let ptr = self.start.as_ptr().add(index * self.object_size);
            Ok(NonNull::new_unchecked(ptr))
        }
    }

    /// 获取FreeNode的可变引用
    fn get_free_node_mut(&self, index: usize) -> Result<&mut FreeNode, SlabError> {
        let ptr = self.get_object_ptr(index)?;
        unsafe {
            Ok(&mut *(ptr.as_ptr() as *mut FreeNode))
        }
    }
}

unsafe impl Send for Slab {}
unsafe impl Sync for Slab {}

unsafe impl Send for SlabCache {}
unsafe impl Sync for SlabCache {}

impl Slab {
    /// Create a new slab for objects of given size
    pub fn new(object_size: usize) -> Result<Self, SlabError> {
        if object_size == 0 || object_size > PAGE_SIZE {
            return Err(SlabError::InvalidLayout);
        }

        // Allocate a page for the slab
        let frame = frame_alloc().ok_or(SlabError::OutOfMemory)?;
        let start = NonNull::new(frame.ppn.get_bytes_array_mut().as_mut_ptr())
            .ok_or(SlabError::OutOfMemory)?;

        // Ensure object size is at least the size of FreeNode
        let actual_object_size = object_size.max(size_of::<FreeNode>());
        let object_count = PAGE_SIZE / actual_object_size;

        if object_count == 0 {
            return Err(SlabError::InvalidLayout);
        }

        let mut slab = Slab {
            start,
            object_size: actual_object_size,
            object_count,
            free_head: None,
            free_count: 0,
            _frame: frame,
            _slab_frame: None,
            next: None,
        };

        // Initialize free list
        slab.init_free_list()?;

        Ok(slab)
    }

    /// Initialize the free list linking all objects - 使用更安全的索引方式
    fn init_free_list(&mut self) -> Result<(), SlabError> {
        self.free_head = None;

        // Link all objects in reverse order using indices
        for i in (0..self.object_count).rev() {
            let node = self.get_free_node_mut(i)?;
            node.next = self.free_head;
            self.free_head = Some(i);
        }

        self.free_count = self.object_count;
        Ok(())
    }

    /// Allocate an object from this slab - 使用安全的索引方式
    pub fn alloc(&mut self) -> Option<NonNull<u8>> {
        if let Some(free_index) = self.free_head {
            if let Ok(node) = self.get_free_node_mut(free_index) {
                self.free_head = node.next;
                self.free_count -= 1;
                self.get_object_ptr(free_index).ok()
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Deallocate an object back to this slab - 使用安全的索引方式
    pub fn dealloc(&mut self, ptr: NonNull<u8>) -> Result<(), SlabError> {
        // 获取对象在slab中的索引
        let index = self.get_object_index(ptr)?;

        // 将对象添加到空闲链表
        let node = self.get_free_node_mut(index)?;
        node.next = self.free_head;
        self.free_head = Some(index);
        self.free_count += 1;

        Ok(())
    }

    /// Check if this slab is empty (all objects are free)
    pub fn is_empty(&self) -> bool {
        self.free_count == self.object_count
    }

    /// Check if this slab is full (no free objects)
    pub fn is_full(&self) -> bool {
        self.free_count == 0
    }

    /// Get number of free objects
    pub fn free_count(&self) -> usize {
        self.free_count
    }

    /// Get object size
    pub fn object_size(&self) -> usize {
        self.object_size
    }
}

/// SLAB cache for managing objects of a specific size - 使用链表避免Vec
pub struct SlabCache {
    /// Object size for this cache
    object_size: usize,
    /// Head of partial slabs list (slabs with free objects)
    partial_head: Option<NonNull<Slab>>,
    /// Head of full slabs list (slabs with no free objects)
    full_head: Option<NonNull<Slab>>,
    /// Head of empty slabs list (slabs with all objects free)
    empty_head: Option<NonNull<Slab>>,
    /// Statistics
    total_slabs: usize,
    total_allocated: usize,
}

impl SlabCache {
    /// Create a new slab cache for objects of given size
    pub const fn new(object_size: usize) -> Self {
        SlabCache {
            object_size,
            partial_head: None,
            full_head: None,
            empty_head: None,
            total_slabs: 0,
            total_allocated: 0,
        }
    }

    /// Allocate an object from this cache - 使用链表避免Vec操作
    pub fn alloc(&mut self) -> Result<NonNull<u8>, SlabError> {
        // Try to allocate from a partial slab
        if let Some(partial_ptr) = self.partial_head {
            unsafe {
                let slab = &mut *partial_ptr.as_ptr();
                if let Some(ptr) = slab.alloc() {
                    self.total_allocated += 1;

                    // If slab becomes full, move it to full list
                    if slab.is_full() {
                        self.partial_head = slab.next;
                        slab.next = self.full_head;
                        self.full_head = Some(partial_ptr);
                    }

                    return Ok(ptr);
                }
            }
        }

        // Try to use an empty slab
        if let Some(empty_ptr) = self.empty_head {
            unsafe {
                let slab = &mut *empty_ptr.as_ptr();
                self.empty_head = slab.next;

                if let Some(ptr) = slab.alloc() {
                    self.total_allocated += 1;

                    // Move to appropriate list based on fullness
                    if slab.is_full() {
                        slab.next = self.full_head;
                        self.full_head = Some(empty_ptr);
                    } else {
                        slab.next = self.partial_head;
                        self.partial_head = Some(empty_ptr);
                    }

                    return Ok(ptr);
                }
            }
        }

        // Need to create a new slab
        self.create_and_alloc_new_slab()
    }

    /// Create a new slab and allocate from it
    fn create_and_alloc_new_slab(&mut self) -> Result<NonNull<u8>, SlabError> {
        // Allocate a frame for the slab structure itself
        let slab_frame = frame_alloc().ok_or(SlabError::OutOfMemory)?;
        let slab_ptr = NonNull::new(slab_frame.ppn.get_bytes_array_mut().as_mut_ptr() as *mut Slab)
            .ok_or(SlabError::OutOfMemory)?;

        // Initialize the slab structure in place
        unsafe {
            let slab = &mut *slab_ptr.as_ptr();
            *slab = Slab::new(self.object_size)?;
            
            // Store the frame tracker in the slab for proper cleanup
            slab._slab_frame = Some(slab_frame);
            
            if let Some(ptr) = slab.alloc() {
                self.total_allocated += 1;
                self.total_slabs += 1;

                // Add to appropriate list based on fullness
                if slab.is_full() {
                    slab.next = self.full_head;
                    self.full_head = Some(slab_ptr);
                } else {
                    slab.next = self.partial_head;
                    self.partial_head = Some(slab_ptr);
                }

                Ok(ptr)
            } else {
                Err(SlabError::OutOfMemory)
            }
        }
    }

    /// Deallocate an object back to this cache - 使用链表实现
    pub fn dealloc(&mut self, ptr: NonNull<u8>) -> Result<(), SlabError> {
        // Search in partial slabs
        let mut current = self.partial_head;
        while let Some(slab_ptr) = current {
            unsafe {
                let slab = &mut *slab_ptr.as_ptr();
                if self.ptr_in_slab(ptr, slab) {
                    slab.dealloc(ptr)?;

                    // If slab becomes empty, move it to empty list
                    if slab.is_empty() {
                        Self::remove_slab_from_list(slab_ptr, &mut self.partial_head);
                        slab.next = self.empty_head;
                        self.empty_head = Some(slab_ptr);
                        self.cleanup_empty_slabs();
                    }

                    return Ok(());
                }
                current = slab.next;
            }
        }

        // Search in full slabs
        let mut current = self.full_head;
        while let Some(slab_ptr) = current {
            unsafe {
                let slab = &mut *slab_ptr.as_ptr();
                if self.ptr_in_slab(ptr, slab) {
                    slab.dealloc(ptr)?;

                    // Full slab now has free space, move to partial list
                    Self::remove_slab_from_list(slab_ptr, &mut self.full_head);
                    
                    if slab.is_empty() {
                        slab.next = self.empty_head;
                        self.empty_head = Some(slab_ptr);
                        self.cleanup_empty_slabs();
                    } else {
                        slab.next = self.partial_head;
                        self.partial_head = Some(slab_ptr);
                    }

                    return Ok(());
                }
                current = slab.next;
            }
        }

        // Search in empty slabs (shouldn't happen but for completeness)
        let mut current = self.empty_head;
        while let Some(slab_ptr) = current {
            unsafe {
                let slab = &mut *slab_ptr.as_ptr();
                if self.ptr_in_slab(ptr, slab) {
                    slab.dealloc(ptr)?;
                    return Ok(());
                }
                current = slab.next;
            }
        }

        Err(SlabError::InvalidPointer)
    }


    /// Remove a slab from a linked list
    fn remove_slab_from_list(target: NonNull<Slab>, head: &mut Option<NonNull<Slab>>) {
        unsafe {
            if let Some(head_ptr) = *head {
                if head_ptr == target {
                    // Remove head
                    *head = (*head_ptr.as_ptr()).next;
                    return;
                }

                let mut current = head_ptr;
                while let Some(next_ptr) = (*current.as_ptr()).next {
                    if next_ptr == target {
                        // Remove next
                        (*current.as_ptr()).next = (*next_ptr.as_ptr()).next;
                        return;
                    }
                    current = next_ptr;
                }
            }
        }
    }

    /// Check if a pointer belongs to a specific slab - 使用更安全的方式
    fn ptr_in_slab(&self, ptr: NonNull<u8>, slab: &Slab) -> bool {
        slab.get_object_index(ptr).is_ok()
    }

    /// Safely deallocate a slab structure that was allocated manually
    unsafe fn deallocate_slab(&mut self, slab_ptr: NonNull<Slab>) {
        // The slab frame will be automatically deallocated when the FrameTracker is dropped
        // We just need to ensure the slab is properly dropped
        unsafe {
            let slab = &mut *slab_ptr.as_ptr();
            
            // Manually drop the slab to run its destructor
            core::ptr::drop_in_place(slab);
        }
        
        // The _slab_frame field will be automatically deallocated when dropped
        self.total_slabs -= 1;
    }

    /// Clean up excess empty slabs to prevent memory waste
    fn cleanup_empty_slabs(&mut self) {
        const MAX_EMPTY_SLABS: usize = 2; // Keep at most 2 empty slabs per cache
        
        let mut empty_count = 0;
        let mut current = self.empty_head;
        
        // Count empty slabs
        while let Some(slab_ptr) = current {
            unsafe {
                empty_count += 1;
                current = (*slab_ptr.as_ptr()).next;
            }
        }
        
        // If we have too many empty slabs, remove some
        if empty_count > MAX_EMPTY_SLABS {
            let to_remove = empty_count - MAX_EMPTY_SLABS;
            
            for _ in 0..to_remove {
                if let Some(slab_ptr) = self.empty_head {
                    unsafe {
                        self.empty_head = (*slab_ptr.as_ptr()).next;
                        self.deallocate_slab(slab_ptr);
                    }
                }
            }
        }
    }
}

/// Common object sizes for SLAB caches
const COMMON_SIZES: &[usize] = &[8, 16, 32, 64, 128, 256, 512, 1024, 2048];

/// Global SLAB allocator
pub struct SlabAllocator {
    /// Caches for common object sizes
    caches: [Mutex<SlabCache>; COMMON_SIZES.len()],
}

impl SlabAllocator {
    /// Create a new SLAB allocator
    pub const fn new() -> Self {
        const INIT_CACHE: Mutex<SlabCache> = Mutex::new(SlabCache::new(0));

        SlabAllocator {
            caches: [INIT_CACHE; COMMON_SIZES.len()],
        }
    }

    /// Initialize the SLAB allocator
    pub fn init(&self) {
        debug!("[SLAB] Initializing SLAB allocator with {} caches", COMMON_SIZES.len());
        for (i, &size) in COMMON_SIZES.iter().enumerate() {
            *self.caches[i].lock() = SlabCache::new(size);
            debug!("[SLAB] Cache {} initialized for size {}", i, size);
        }
        debug!("[SLAB] SLAB allocator initialization complete");
    }

    /// Find the appropriate cache for a given layout
    fn find_cache_index(&self, layout: Layout) -> Option<usize> {
        let size = layout.size();
        let align = layout.align();

        for (i, &cache_size) in COMMON_SIZES.iter().enumerate() {
            if size <= cache_size && align <= cache_size {
                return Some(i);
            }
        }
        None
    }

    /// Allocate memory using SLAB allocator
    pub fn alloc(&self, layout: Layout) -> Result<NonNull<u8>, SlabError> {
        if let Some(cache_idx) = self.find_cache_index(layout) {
            let result = self.caches[cache_idx].lock().alloc();
            if result.is_err() {
                debug!("[SLAB] Failed to allocate {} bytes from cache {}", layout.size(), cache_idx);
            }
            result
        } else {
            debug!("[SLAB] No suitable cache for layout: size={}, align={}", layout.size(), layout.align());
            Err(SlabError::InvalidLayout)
        }
    }

    /// Deallocate memory using SLAB allocator
    pub fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) -> Result<(), SlabError> {
        if let Some(cache_idx) = self.find_cache_index(layout) {
            let result = self.caches[cache_idx].lock().dealloc(ptr);
            if result.is_err() {
                debug!("[SLAB] Failed to deallocate ptr {:p} from cache {}", ptr.as_ptr(), cache_idx);
            }
            result
        } else {
            debug!("[SLAB] No suitable cache for dealloc: size={}, align={}", layout.size(), layout.align());
            Err(SlabError::InvalidPointer)
        }
    }
}

/// Global SLAB allocator instance
pub static SLAB_ALLOCATOR: SlabAllocator = SlabAllocator::new();