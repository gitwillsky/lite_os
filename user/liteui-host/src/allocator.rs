use core::{alloc::Layout, ptr};

use crate::ffi;

struct MuslAllocator;

// SAFETY: liteui-host 只有这一条 Rust→musl allocation seam。malloc 满足
// max_align_t；更高对齐经 aligned_alloc，且 size 先取整到 alignment 倍数。
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

// OWNER: host Rust scaffolding 的唯一 allocator。QuickJS heap 由其 Runtime 内部
// allocator 与 JS_SetMemoryLimit 独立拥有；混用会让 JS quota 失真。
#[global_allocator]
static ALLOCATOR: MuslAllocator = MuslAllocator;

#[alloc_error_handler]
fn allocation_failure(_layout: Layout) -> ! {
    ffi::write_stderr(b"liteui-host: allocation failure\n");
    unsafe { ffi::_exit(125) }
}
