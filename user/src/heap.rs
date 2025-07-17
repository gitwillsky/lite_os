use crate::syscall::{brk, sbrk};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::null_mut;

/// 简单的链表节点用于管理空闲内存块
#[repr(C)]
struct FreeBlock {
    size: usize,
    next: *mut FreeBlock,
}

/// 基于内核系统调用的堆分配器
pub struct KernelHeapAllocator {
    // 使用静态变量存储堆状态，避免初始化问题
}

static mut HEAP_START: usize = 0;
static mut HEAP_END: usize = 0;
static mut FREE_LIST: *mut FreeBlock = null_mut();

impl KernelHeapAllocator {
    pub const fn new() -> Self {
        KernelHeapAllocator {}
    }

    /// 初始化堆
    pub fn init(&self) {
        unsafe {
            // 获取当前的堆顶地址
            let current_brk = brk(0);
            if current_brk > 0 {
                HEAP_START = current_brk as usize;
                HEAP_END = current_brk as usize;
                FREE_LIST = null_mut();

                // 立即分配一个小的初始堆空间
                let initial_size = 4096; // 4KB
                if self.expand_heap(initial_size) {
                    // 堆初始化成功
                } else {
                    // 堆初始化失败，但不要panic，让后续分配尝试扩展
                }
            }
        }
    }

    /// 扩展堆大小
    fn expand_heap(&self, size: usize) -> bool {
            // 页对齐大小
            let page_size = 4096;
            let aligned_size = (size + page_size - 1) & !(page_size - 1);

            let old_brk = sbrk(aligned_size as isize);
            if old_brk > 0 {
                true
            } else {
                false
            }
    }

    /// 分配内存
    fn alloc_memory(&self, size: usize) -> *mut u8 {
        unsafe {
            let aligned_size =
                (size + core::mem::size_of::<usize>() - 1) & !(core::mem::size_of::<usize>() - 1);

            // 首先尝试从空闲链表中分配
            if let Some(ptr) = self.alloc_from_free_list(aligned_size) {
                return ptr;
            }

            // 如果空闲链表中没有合适的块，扩展堆
            let total_size = aligned_size + core::mem::size_of::<usize>();

            // 检查是否需要扩展堆
            if HEAP_START + total_size > HEAP_END {
                if !self.expand_heap(total_size) {
                    return null_mut();
                }
            }

            // 从堆顶分配
            let ptr = HEAP_START as *mut u8;
            *(ptr as *mut usize) = aligned_size; // 存储大小信息
            let result_ptr = ptr.add(core::mem::size_of::<usize>());
            HEAP_START += total_size;


            result_ptr
        }
    }

    /// 从空闲链表中分配内存
    fn alloc_from_free_list(&self, size: usize) -> Option<*mut u8> {
        unsafe {
            let mut current = FREE_LIST;
            let mut prev: *mut FreeBlock = null_mut();
            let total_size = size + core::mem::size_of::<usize>(); // 需要的总大小

            while !current.is_null() {
                let block = &mut *current;

                if block.size >= total_size {

                    // 找到合适的块
                    if prev.is_null() {
                        FREE_LIST = block.next;
                    } else {
                        (*prev).next = block.next;
                    }

                    // 如果块太大，分割它
                    if block.size > total_size + core::mem::size_of::<FreeBlock>() {
                        let new_block = (current as *mut u8).add(total_size) as *mut FreeBlock;
                        (*new_block).size = block.size - total_size;
                        (*new_block).next = FREE_LIST;
                        FREE_LIST = new_block;
                    }

                    // 在分配的块前面存储大小信息，保持与alloc_memory一致
                    *(current as *mut usize) = size;
                    return Some((current as *mut u8).add(core::mem::size_of::<usize>()));
                }

                prev = current;
                current = block.next;
            }

            None
        }
    }

    /// 释放内存
    fn free_memory(&self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }

        unsafe {
            // 获取块的大小信息，ptr指向的是用户数据，size存储在ptr-8位置
            let size_ptr = ptr.sub(core::mem::size_of::<usize>()) as *mut usize;
            let size = *size_ptr;

            // 将块添加到空闲链表，block指向包含size信息的完整块
            let block = size_ptr as *mut FreeBlock;
            (*block).size = size + core::mem::size_of::<usize>(); // 存储包含size头部的总大小
            (*block).next = FREE_LIST;
            FREE_LIST = block;
        }
    }
}

unsafe impl GlobalAlloc for KernelHeapAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // 初始化堆（如果还没有初始化）
        unsafe {
            if HEAP_START == 0 {
                self.init();
                // 如果初始化后仍然是0，说明初始化失败
                if HEAP_START == 0 {
                    return null_mut();
                }
            }
        }

        let ptr = self.alloc_memory(layout.size());
        if ptr.is_null() {
            panic!("alloc failed");
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        self.free_memory(ptr);
    }
}

/// 全局堆分配器实例
pub static HEAP_ALLOCATOR: KernelHeapAllocator = KernelHeapAllocator::new();

/// 分配错误处理
#[alloc_error_handler]
pub fn handle_alloc_error(layout: Layout) -> ! {
    panic!(
        "Memory allocation failed: size={}, align={}",
        layout.size(),
        layout.align()
    );
}
