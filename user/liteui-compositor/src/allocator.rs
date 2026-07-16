use core::{alloc::Layout, ptr};

use crate::ffi;

struct MuslAllocator;

// SAFETY: 这是 compositor 进程唯一的 Rust allocator adapter。musl malloc 提供
// max_align_t 对齐；更高的 Rust Layout 对齐由 aligned_alloc 显式满足，且长度向上
// 取整为 alignment 的整数倍。所有返回指针只交还给同一 musl free。
unsafe impl core::alloc::GlobalAlloc for MuslAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let size = layout.size().max(1);
        if layout.align() <= core::mem::align_of::<u128>() {
            return unsafe { ffi::malloc(size).cast() };
        }
        let Some(aligned_size) = size
            .checked_add(layout.align() - 1)
            .map(|value| value & !(layout.align() - 1))
        else {
            return ptr::null_mut();
        };
        unsafe { ffi::aligned_alloc(layout.align(), aligned_size).cast() }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, _layout: Layout) {
        unsafe { ffi::free(pointer.cast()) }
    }
}

// OWNER: compositor 唯一全局 allocator；新增第二 adapter 会让分配与释放跨 libc
// domain。失败后必须终止进程，因为恢复场景的双状态预分配不完整便无法原子发布。
#[global_allocator]
static ALLOCATOR: MuslAllocator = MuslAllocator;

#[alloc_error_handler]
fn allocation_failure(_layout: Layout) -> ! {
    let message = b"liteui-compositor: allocation failure\n";
    unsafe {
        ffi::write(2, message.as_ptr().cast(), message.len());
        ffi::_exit(125)
    }
}
