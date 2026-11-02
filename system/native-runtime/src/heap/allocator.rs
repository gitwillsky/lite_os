//! 系统服务目标的 VMAR 分配器；宿主夹具使用宿主分配器。

use super::{VMAR, vmar};
use crate::Vmo;
use crate::sync::Mutex;
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{NonNull, null_mut};

const MIN_CLASS_SHIFT: u32 = 4;
const SMALL_CLASS_COUNT: usize = 8;
const SMALL_MAX_SIZE: usize = 1 << (MIN_CLASS_SHIFT as usize + SMALL_CLASS_COUNT - 1);
// 整页与完整 channel 缓冲各只保留一个备用映射，不保留并发峰值。
const CHUNK_SIZE: usize = native_abi::CHANNEL_MAX_BYTES;
const PAGE_BUFFER_CLASS: usize = SMALL_CLASS_COUNT;
const CHANNEL_BUFFER_CLASS: usize = SMALL_CLASS_COUNT + 1;
const CLASS_COUNT: usize = SMALL_CLASS_COUNT + 2;
const _: () = assert!(CHUNK_SIZE.is_power_of_two() && CHUNK_SIZE >= native_abi::PAGE_SIZE);

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
    if size <= SMALL_MAX_SIZE {
        Some((size.next_power_of_two().trailing_zeros() - MIN_CLASS_SHIFT) as usize)
    } else if size <= native_abi::PAGE_SIZE {
        Some(PAGE_BUFFER_CLASS)
    } else if size == CHUNK_SIZE {
        Some(CHANNEL_BUFFER_CLASS)
    } else {
        None
    }
}

fn class_size(class: usize) -> usize {
    match class {
        PAGE_BUFFER_CLASS => native_abi::PAGE_SIZE,
        CHANNEL_BUFFER_CLASS => CHUNK_SIZE,
        _ => 1usize << (class as u32 + MIN_CLASS_SHIFT),
    }
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
        .map(
            native_abi::vm::READ | native_abi::vm::WRITE,
            0,
            &vmo,
            0,
            total,
        )
        .ok()?;
    let start = base.next_multiple_of(align.max(page));
    let end = start + size;
    // SAFETY: 整段映射尚未发表；头尾均在其内，不存在调用者别名。
    let trimmed = unsafe {
        let prefix = if start > base {
            vmar.unmap(base, start - base)
        } else {
            Ok(())
        };
        prefix.and_then(|()| {
            if base + total > end {
                vmar.unmap(end, base + total - end)
            } else {
                Ok(())
            }
        })
    };
    if trimmed.is_err() {
        // SAFETY: 仍独占未发表区间；unmap 允许覆盖已裁掉的洞，撤销剩余映射后才能报告分配失败。
        unsafe { vmar.unmap(base, total) }
            .expect("unpublished heap mapping rollback must complete");
        return None;
    }
    Some(start)
}

/// 父 heap 模块注册的服务分配器；各实例只访问同一个进程内 Classes owner。
pub(super) struct Heap;

// SAFETY: 小块来自独占的空闲链，大块来自独立映射；同一地址不会同时交给两个调用者。
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(class) = class_of(layout) else {
            return map(layout.size(), layout.align())
                .map_or(null_mut(), |address| address as *mut u8);
        };
        let block_size = class_size(class);
        let mut classes = CLASSES.lock();
        if classes.free[class].is_none() {
            // 整页消息（例如带头部的半页 PCM）独占映射；否则 slab 会永久保留短时的并发峰值。
            let chunk_size = if class >= SMALL_CLASS_COUNT {
                block_size
            } else {
                CHUNK_SIZE
            };
            let Some(chunk) = map(chunk_size, block_size.max(native_abi::PAGE_SIZE)) else {
                return null_mut();
            };
            // 把新区块切成块并串入空闲链（逆序，使低地址先被取出）。
            for index in (0..chunk_size / block_size).rev() {
                let block = (chunk + index * block_size) as *mut FreeBlock;
                // SAFETY: 块位于刚映射的可写区块内，按块大小对齐。
                unsafe {
                    block.write(FreeBlock {
                        next: classes.free[class],
                    })
                };
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
            unsafe { vmar().unmap(pointer as usize, size) }
                .expect("heap deallocation must retire its mapping");
            return;
        };
        let mut classes = CLASSES.lock();
        if class >= SMALL_CLASS_COUNT && classes.free[class].is_some() {
            // 1. 整页/完整缓冲独占映射，不与小块共享。2. 本桶已有一个备用缓冲时直接退休
            // 当前映射；必须先释放 Classes 锁，再执行可能等待翻译 fence 的 unmap。
            drop(classes);
            // SAFETY: 调用者归还独占映射，地址已按块大小对齐，没有其他活跃块。
            unsafe { vmar().unmap(pointer as usize, class_size(class)) }
                .expect("surplus message buffer must retire its mapping");
            return;
        }
        let block = pointer.cast::<FreeBlock>();
        // SAFETY: 调用者归还的块至少有一个 `FreeBlock` 大小且按其对齐。
        unsafe {
            block.write(FreeBlock {
                next: classes.free[class],
            })
        };
        classes.free[class] = NonNull::new(block);
    }
}
