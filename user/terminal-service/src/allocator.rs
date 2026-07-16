use core::{alloc::Layout, ptr};

use crate::ffi;

struct MuslAllocator;

// SAFETY: terminal-service 的 Rust allocations 只经该 musl adapter 创建和释放；
// over-aligned layouts 使用满足 C11 size multiple 的 aligned_alloc。
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

// OWNER: protocol frame 是该进程唯一 Rust heap state；分配失败后无法证明
// grid snapshot publication 完整，必须退出交由 session 重建。
#[global_allocator]
static ALLOCATOR: MuslAllocator = MuslAllocator;

#[alloc_error_handler]
fn allocation_failure(_layout: Layout) -> ! {
    ffi::write_stderr(b"terminal-service: allocation failure\n");
    unsafe { ffi::_exit(125) }
}
