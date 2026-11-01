//! 系统服务目标的 VMAR 分配器；宿主夹具使用宿主分配器。

use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{NonNull, null_mut};
use crate::sync::Mutex;
use crate::Vmo;
use super::{VMAR, vmar};

const MIN_CLASS_SHIFT: u32 = 4;
const CLASS_COUNT: usize = 8;
const LARGEST_CLASS: usize = 1 << (MIN_CLASS_SHIFT as usize + CLASS_COUNT - 1);
const CHUNK_SIZE: usize = 64 * 1024;

struct FreeBlock {
    next: Option<NonNull<FreeBlock>>,
}

struct Classes {
    free: [Option<NonNull<FreeBlock>>; CLASS_COUNT],
}

// SAFETY: 空闲链只在 `Mutex` 内访问，链上的块属于本分配器。
unsafe impl Send for Classes {}

// OWNER: 各级空闲链；只在锁内修改。
static CLASSES: Mutex<Classes> = Mutex::new(Classes {
    free: [None; CLASS_COUNT],
});


fn class_of(layout: Layout) -> Option<usize> {
    let size = layout.size().max(layout.align()).max(1 << MIN_CLASS_SHIFT);
    if size > LARGEST_CLASS {
        return None;
    }
    Some((size.next_power_of_two().trailing_zeros() - MIN_CLASS_SHIFT) as usize)
}

/// 映射 `size` 字节的新匿名内存，地址按 `align` 对齐。
fn map(size: usize, align: usize) -> Option<usize> {
    let vmar = VMAR.get()?;
    let page = native_abi::PAGE_SIZE;
    let size = size.next_multiple_of(page);
    let extra = align.saturating_sub(page);
    let total = size.checked_add(extra)?;
    let vmo = Vmo::create(total as u64, 0).ok()?;
    let base = vmar
        .map(native_abi::vm::READ | native_abi::vm::WRITE, 0, &vmo, 0, total)
        .ok()?;
    let start = base.next_multiple_of(align.max(page));
    // SAFETY: 头尾富余区间刚映射、从未交给任何调用者。
    unsafe {
        if start > base {
            let _ = vmar.unmap(base, start - base);
        }
        let end = start + size;
        if base + total > end {
            let _ = vmar.unmap(end, base + total - end);
        }
    }
    Some(start)
}

struct Heap;

// SAFETY: 小块来自独占的空闲链，大块来自独立映射；同一地址不会同时交给两个调用者。
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = class_of(layout) else {
            return map(layout.size(), layout.align()).map_or(null_mut(), |address| address as *mut u8);
        };
        let block_size = 1usize << (class as u32 + MIN_CLASS_SHIFT);
        let mut classes = CLASSES.lock();
        if classes.free[class].is_none() {
            let Some(chunk) = map(CHUNK_SIZE, native_abi::PAGE_SIZE) else {
                return null_mut();
            };
            // 把新区块切成块并串入空闲链（逆序，使低地址先被取出）。
            for index in (0..CHUNK_SIZE / block_size).rev() {
                let block = (chunk + index * block_size) as *mut FreeBlock;
                // SAFETY: 块位于刚映射的可写区块内，按块大小对齐。
                unsafe { block.write(FreeBlock { next: classes.free[class] }) };
                classes.free[class] = NonNull::new(block);
            }
        }
        let block = classes.free[class].expect("class refilled above");
        // SAFETY: 空闲链上的块都是有效的 `FreeBlock`。
        classes.free[class] = unsafe { block.as_ref().next };
        block.as_ptr().cast()
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let Some(class) = class_of(layout) else {
            let size = layout.size().next_multiple_of(native_abi::PAGE_SIZE);
            // SAFETY: 大块是独立映射，调用者保证不再使用。
            let _ = unsafe { vmar().unmap(pointer as usize, size) };
            return;
        };
        let mut classes = CLASSES.lock();
        let block = pointer.cast::<FreeBlock>();
        // SAFETY: 调用者归还的块至少有一个 `FreeBlock` 大小且按其对齐。
        unsafe { block.write(FreeBlock { next: classes.free[class] }) };
        classes.free[class] = NonNull::new(block);
    }
}

#[global_allocator]
static HEAP: Heap = Heap;
