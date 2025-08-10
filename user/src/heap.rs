use crate::syscall::{brk, sbrk};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;
use core::mem::size_of;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::cell::UnsafeCell;

const PAGE_SIZE: usize = 4096;
const MIN_BLOCK_SIZE: usize = 32;
const ALIGNMENT: usize = 8;

// 分配器统计信息
#[derive(Debug, Default)]
struct AllocatorStats {
    total_allocated: AtomicUsize,
    total_freed: AtomicUsize,
    current_usage: AtomicUsize,
    heap_size: AtomicUsize,
    allocation_count: AtomicUsize,
    free_count: AtomicUsize,
    expand_count: AtomicUsize,
    shrink_count: AtomicUsize,
}

impl AllocatorStats {
    const fn new() -> Self {
        Self {
            total_allocated: AtomicUsize::new(0),
            total_freed: AtomicUsize::new(0),
            current_usage: AtomicUsize::new(0),
            heap_size: AtomicUsize::new(0),
            allocation_count: AtomicUsize::new(0),
            free_count: AtomicUsize::new(0),
            expand_count: AtomicUsize::new(0),
            shrink_count: AtomicUsize::new(0),
        }
    }

    fn record_alloc(&self, size: usize) {
        self.total_allocated.fetch_add(size, Ordering::Relaxed);
        self.current_usage.fetch_add(size, Ordering::Relaxed);
        self.allocation_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_free(&self, size: usize) {
        self.total_freed.fetch_add(size, Ordering::Relaxed);
        self.current_usage.fetch_sub(size, Ordering::Relaxed);
        self.free_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_expand(&self, size: usize) {
        self.heap_size.fetch_add(size, Ordering::Relaxed);
        self.expand_count.fetch_add(1, Ordering::Relaxed);
    }

    fn record_shrink(&self, size: usize) {
        self.heap_size.fetch_sub(size, Ordering::Relaxed);
        self.shrink_count.fetch_add(1, Ordering::Relaxed);
    }
}

/// 改进的空闲块结构，支持双向链表和大小分类
#[repr(C)]
struct FreeBlock {
    size: usize,              // 块大小（包含头部）
    prev: *mut FreeBlock,     // 前一个空闲块
    next: *mut FreeBlock,     // 后一个空闲块
    magic: u32,               // 魔数用于调试和验证
}

impl FreeBlock {
    const MAGIC: u32 = 0xDEADBEEF;

    fn new(size: usize) -> Self {
        Self {
            size,
            prev: null_mut(),
            next: null_mut(),
            magic: Self::MAGIC,
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == Self::MAGIC && self.size >= size_of::<FreeBlock>()
    }

    fn get_data_ptr(&mut self) -> *mut u8 {
        unsafe { (self as *mut FreeBlock).add(1) as *mut u8 }
    }

    fn from_data_ptr(ptr: *mut u8) -> *mut FreeBlock {
        unsafe { (ptr as *mut FreeBlock).sub(1) }
    }
}

/// 分配的块头部信息
#[repr(C)]
struct AllocatedBlock {
    size: usize,    // 实际分配的大小
    magic: u32,     // 魔数
}

impl AllocatedBlock {
    const MAGIC: u32 = 0xCAFEBABE;

    fn new(size: usize) -> Self {
        Self {
            size,
            magic: Self::MAGIC,
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == Self::MAGIC
    }

    fn get_data_ptr(&mut self) -> *mut u8 {
        unsafe { (self as *mut AllocatedBlock).add(1) as *mut u8 }
    }

    fn from_data_ptr(ptr: *mut u8) -> *mut AllocatedBlock {
        unsafe { (ptr as *mut AllocatedBlock).sub(1) }
    }
}

/// 大小类别数组，用于快速查找合适的空闲块
const SIZE_CLASSES: [usize; 16] = [
    32, 64, 128, 256, 512, 1024, 2048, 4096,
    8192, 16384, 32768, 65536, 131072, 262144, 524288, usize::MAX
];

/// 高效的用户态堆分配器
pub struct AdvancedHeapAllocator {
    // 堆边界
    heap_start: AtomicUsize,
    heap_end: AtomicUsize,

    // 分大小类别的空闲链表头 (使用UnsafeCell以支持内部可变性)
    free_lists: UnsafeCell<[*mut FreeBlock; SIZE_CLASSES.len()]>,

    // 统计信息
    stats: AllocatorStats,

    // 初始化标志
    initialized: AtomicUsize,
}

// 手动实现Sync，因为我们知道这在单线程用户空间环境中是安全的
unsafe impl Sync for AdvancedHeapAllocator {}

impl AdvancedHeapAllocator {
    pub const fn new() -> Self {
        const NULL_PTR: *mut FreeBlock = null_mut();
        Self {
            heap_start: AtomicUsize::new(0),
            heap_end: AtomicUsize::new(0),
            free_lists: UnsafeCell::new([NULL_PTR; SIZE_CLASSES.len()]),
            stats: AllocatorStats::new(),
            initialized: AtomicUsize::new(0),
        }
    }

    /// 初始化堆分配器
    pub fn init(&self) {
        // 使用CAS确保只初始化一次
        if self.initialized.compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            self.do_init();
        }
    }

    fn do_init(&self) {
        // 获取当前的brk
        let current_brk = brk(0);
        if current_brk <= 0 {
            return;
        }

        self.heap_start.store(current_brk as usize, Ordering::Release);
        self.heap_end.store(current_brk as usize, Ordering::Release);

        // 分配初始堆空间
        let initial_size = 8 * PAGE_SIZE; // 32KB 初始大小
        if self.expand_heap_internal(initial_size) {
            self.stats.record_expand(initial_size);
        }
    }

    /// 根据大小获取对应的size class索引
    fn get_size_class_index(&self, size: usize) -> usize {
        for (i, &class_size) in SIZE_CLASSES.iter().enumerate() {
            if size <= class_size {
                return i;
            }
        }
        SIZE_CLASSES.len() - 1
    }

    /// 对齐大小到指定边界
    fn align_size(&self, size: usize, align: usize) -> usize {
        (size + align - 1) & !(align - 1)
    }

    /// 计算总的块大小（包含头部）
    fn calculate_total_size(&self, size: usize) -> usize {
        let aligned_size = self.align_size(size, ALIGNMENT);
        let total_size = size_of::<AllocatedBlock>() + aligned_size;
        self.align_size(total_size.max(MIN_BLOCK_SIZE), ALIGNMENT)
    }

    /// 扩展堆
    fn expand_heap_internal(&self, min_size: usize) -> bool {
        let aligned_size = self.align_size(min_size, PAGE_SIZE);
        let old_brk = sbrk(aligned_size as isize);

        if old_brk > 0 {
            let old_end = self.heap_end.load(Ordering::Acquire);
            let new_end = old_end + aligned_size;
            self.heap_end.store(new_end, Ordering::Release);

            // 将新分配的内存添加到空闲链表
            self.add_free_block(old_end, aligned_size);
            true
        } else {
            false
        }
    }

    /// 收缩堆（将未使用的内存归还给系统）
    fn shrink_heap_if_possible(&self) {
        let heap_start = self.heap_start.load(Ordering::Acquire);
        let heap_end = self.heap_end.load(Ordering::Acquire);
        let heap_size = heap_end - heap_start;

        // 只有当堆很大且使用率很低时才收缩
        let current_usage = self.stats.current_usage.load(Ordering::Relaxed);
        let usage_ratio = if heap_size > 0 { (current_usage * 100) / heap_size } else { 100 };

        // 如果使用率低于25%且堆大于1MB，尝试收缩
        if usage_ratio < 25 && heap_size > 1024 * 1024 {
            let shrink_size = self.find_shrinkable_memory();
            if shrink_size >= PAGE_SIZE * 4 { // 至少收缩16KB
                if sbrk(-(shrink_size as isize)) > 0 {
                    self.heap_end.fetch_sub(shrink_size, Ordering::Release);
                    self.stats.record_shrink(shrink_size);
                }
            }
        }
    }

    /// 查找可以收缩的内存大小
    fn find_shrinkable_memory(&self) -> usize {
        let heap_end = self.heap_end.load(Ordering::Acquire);
        let mut shrink_size = 0;

        // 从堆尾部开始查找连续的空闲块
        for &class_size in SIZE_CLASSES.iter().rev() {
            let class_index = self.get_size_class_index(class_size);
            let free_lists = unsafe { &mut *self.free_lists.get() };
            let mut current = free_lists[class_index];

            while !current.is_null() {
                unsafe {
                    let block = &*current;
                    if !block.is_valid() {
                        break;
                    }

                    let block_addr = current as usize;
                    let block_end = block_addr + block.size;

                    // 如果这个块在堆的末尾
                    if block_end == heap_end {
                        shrink_size += block.size;
                        // 从链表中移除这个块
                        self.remove_from_free_list(current);
                    }

                    current = block.next;
                }
            }
        }

        // 确保收缩大小页对齐
        self.align_size(shrink_size, PAGE_SIZE)
    }

    /// 添加空闲块到链表
    fn add_free_block(&self, addr: usize, size: usize) {
        if size < size_of::<FreeBlock>() {
            return;
        }

        let block_ptr = addr as *mut FreeBlock;
        unsafe {
            let block = &mut *block_ptr;
            *block = FreeBlock::new(size);

            // 尝试与相邻块合并
            self.coalesce_block(block_ptr);
        }
    }

    /// 合并相邻的空闲块
    fn coalesce_block(&self, block_ptr: *mut FreeBlock) {
        unsafe {
            let block = &mut *block_ptr;
            if !block.is_valid() {
                return;
            }

            let block_addr = block_ptr as usize;
            let block_end = block_addr + block.size;
            let heap_start = self.heap_start.load(Ordering::Acquire);
            let heap_end = self.heap_end.load(Ordering::Acquire);

            // 向前合并
            if block_addr > heap_start {
                if let Some(prev_block) = self.find_previous_block(block_addr) {
                    let prev_addr = prev_block as usize;
                    let prev_end = prev_addr + (*prev_block).size;

                    if prev_end == block_addr && (*prev_block).is_valid() {
                        // 合并前一个块
                        self.remove_from_free_list(prev_block);
                        (*prev_block).size += block.size;
                        self.add_to_free_list(prev_block);
                        return;
                    }
                }
            }

            // 向后合并
            if block_end < heap_end {
                let next_block_ptr = block_end as *mut FreeBlock;
                if self.is_free_block(next_block_ptr) {
                    let next_block = &mut *next_block_ptr;
                    if next_block.is_valid() {
                        // 合并后一个块
                        self.remove_from_free_list(next_block_ptr);
                        block.size += next_block.size;
                    }
                }
            }

            // 将合并后的块添加到链表
            self.add_to_free_list(block_ptr);
        }
    }

    /// 检查指定地址是否是空闲块
    fn is_free_block(&self, ptr: *mut FreeBlock) -> bool {
        if ptr.is_null() {
            return false;
        }

        // 检查所有空闲链表
        let free_lists = unsafe { &*self.free_lists.get() };
        for &head in free_lists {
            let mut current = head;
            while !current.is_null() {
                if current == ptr {
                    return true;
                }
                unsafe {
                    current = (*current).next;
                }
            }
        }
        false
    }

    /// 查找前一个块
    fn find_previous_block(&self, addr: usize) -> Option<*mut FreeBlock> {
        let heap_start = self.heap_start.load(Ordering::Acquire);

        // 遍历所有空闲块，找到结束地址等于当前地址的块
        let free_lists = unsafe { &*self.free_lists.get() };
        for &head in free_lists {
            let mut current = head;
            while !current.is_null() {
                unsafe {
                    let block = &*current;
                    if !block.is_valid() {
                        break;
                    }

                    let block_addr = current as usize;
                    let block_end = block_addr + block.size;

                    if block_end == addr && block_addr >= heap_start {
                        return Some(current);
                    }

                    current = block.next;
                }
            }
        }
        None
    }

    /// 将块添加到对应的空闲链表
    fn add_to_free_list(&self, block_ptr: *mut FreeBlock) {
        unsafe {
            let block = &mut *block_ptr;
            if !block.is_valid() {
                return;
            }

            let class_index = self.get_size_class_index(block.size);
            let free_lists = unsafe { &mut *self.free_lists.get() };
            let old_head = free_lists[class_index];

            block.next = old_head;
            block.prev = null_mut();

            if !old_head.is_null() {
                (*old_head).prev = block_ptr;
            }

            free_lists[class_index] = block_ptr;
        }
    }

    /// 从空闲链表中移除块
    fn remove_from_free_list(&self, block_ptr: *mut FreeBlock) {
        unsafe {
            let block = &mut *block_ptr;
            if !block.is_valid() {
                return;
            }

            let class_index = self.get_size_class_index(block.size);
            let free_lists = unsafe { &mut *self.free_lists.get() };

            // 更新前一个节点的next指针
            if !block.prev.is_null() {
                (*block.prev).next = block.next;
            } else {
                // 这是头节点
                free_lists[class_index] = block.next;
            }

            // 更新后一个节点的prev指针
            if !block.next.is_null() {
                (*block.next).prev = block.prev;
            }

            block.prev = null_mut();
            block.next = null_mut();
        }
    }

    /// 从空闲链表中查找合适的块
    fn find_free_block(&self, size: usize) -> Option<*mut FreeBlock> {
        let class_index = self.get_size_class_index(size);

        // 从对应的size class开始查找
        let free_lists = unsafe { &*self.free_lists.get() };
        for i in class_index..SIZE_CLASSES.len() {
            let mut current = free_lists[i];

            while !current.is_null() {
                unsafe {
                    let block = &*current;
                    if !block.is_valid() {
                        break;
                    }

                    if block.size >= size {
                        return Some(current);
                    }
                    current = block.next;
                }
            }
        }
        None
    }

    /// 分割空闲块
    fn split_block(&self, block_ptr: *mut FreeBlock, needed_size: usize) -> *mut u8 {
        unsafe {
            let block = &mut *block_ptr;
            if !block.is_valid() || block.size < needed_size {
                return null_mut();
            }

            // 从空闲链表中移除
            self.remove_from_free_list(block_ptr);

            let remaining_size = block.size - needed_size;

            // 如果剩余大小足够大，分割块
            if remaining_size >= size_of::<FreeBlock>() + MIN_BLOCK_SIZE {
                let new_block_addr = (block_ptr as usize) + needed_size;
                let new_block_ptr = new_block_addr as *mut FreeBlock;
                let new_block = &mut *new_block_ptr;

                *new_block = FreeBlock::new(remaining_size);
                self.add_to_free_list(new_block_ptr);

                // 更新原块大小
                block.size = needed_size;
            }

            // 将空闲块转换为分配块
            let allocated_block = block_ptr as *mut AllocatedBlock;
            let allocated = &mut *allocated_block;
            *allocated = AllocatedBlock::new(block.size);

            allocated.get_data_ptr()
        }
    }

    /// 分配内存
    fn alloc_memory(&self, layout: Layout) -> *mut u8 {
        if !self.is_initialized() {
            self.init();
        }

        let size = layout.size();
        if size == 0 {
            return null_mut();
        }

        let total_size = self.calculate_total_size(size);

        // 首先尝试从空闲链表分配
        if let Some(block_ptr) = self.find_free_block(total_size) {
            let ptr = self.split_block(block_ptr, total_size);
            if !ptr.is_null() {
                self.stats.record_alloc(size);
                return ptr;
            }
        }

        // 如果没有合适的空闲块，扩展堆
        // 避免对大块分配成倍扩容导致一次申请过多内存（例如为32MiB分配直接申请64MiB）
        // 这里按需扩容：至少满足本次需求，且不低于最小增长粒度（16KB）
        let expand_size = core::cmp::max(total_size, PAGE_SIZE * 4);
        if self.expand_heap_internal(expand_size) {
            self.stats.record_expand(expand_size);

            // 再次尝试分配
            if let Some(block_ptr) = self.find_free_block(total_size) {
                let ptr = self.split_block(block_ptr, total_size);
                if !ptr.is_null() {
                    self.stats.record_alloc(size);
                    return ptr;
                }
            }
        }

        null_mut()
    }

    /// 释放内存
    fn free_memory(&self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }

        unsafe {
            let allocated_block = AllocatedBlock::from_data_ptr(ptr);
            let allocated = &*allocated_block;

            if !allocated.is_valid() {
                // 无效的块，可能是double free或corruption
                return;
            }

            let size = allocated.size;
            self.stats.record_free(size - size_of::<AllocatedBlock>());

            // 转换回空闲块并添加到链表
            let block_ptr = allocated_block as *mut FreeBlock;
            let block = &mut *block_ptr;
            *block = FreeBlock::new(size);

            self.coalesce_block(block_ptr);
        }

        // 定期检查是否可以收缩堆
        if self.stats.free_count.load(Ordering::Relaxed) % 100 == 0 {
            self.shrink_heap_if_possible();
        }
    }

    /// 检查是否已初始化
    fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire) != 0
    }

    /// 获取统计信息
    pub fn get_stats(&self) -> (usize, usize, usize, usize) {
        (
            self.stats.current_usage.load(Ordering::Relaxed),
            self.stats.heap_size.load(Ordering::Relaxed),
            self.stats.allocation_count.load(Ordering::Relaxed),
            self.stats.free_count.load(Ordering::Relaxed),
        )
    }
}

unsafe impl GlobalAlloc for AdvancedHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.alloc_memory(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        self.free_memory(ptr);
    }
}

/// 全局堆分配器实例
#[global_allocator]
pub static HEAP_ALLOCATOR: AdvancedHeapAllocator = AdvancedHeapAllocator::new();

/// 分配错误处理
#[alloc_error_handler]
pub fn handle_alloc_error(layout: Layout) -> ! {
    let (usage, heap_size, alloc_count, free_count) = HEAP_ALLOCATOR.get_stats();
    panic!(
        "Memory allocation failed: size={}, align={}, usage={}/{}, allocs={}, frees={}",
        layout.size(), layout.align(), usage, heap_size, alloc_count, free_count
    );
}