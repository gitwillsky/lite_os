//! 系统服务目标的全局分配器：[`span_heap::Heap`] 接线到运行时 VMAR、VMO 与线程描述符。
//!
//! 区域是一个整段映射、按需提交的 VMO；空闲 span 用 `OP_DECOMMIT` 归还物理页而不 unmap，
//! 地址空间保持稳定，也不需要等待翻译 fence 的 unmap。大于 [`span_heap::classes::MAX_SMALL`]
//! 的对象仍使用独立映射。

use super::{VMAR, vmar};
use crate::Vmo;
use crate::sync::Mutex;
use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicPtr, AtomicU32};
use span_heap::{Backing, Heap as SpanHeap, LocalHeap};

/// 同时存在的区域数上限（与 span-heap 的区域表一致，外加堆池块）。
const REGION_SLOTS: usize = 320;

/// 区域 VMO 与区域起点在 VMO 内的偏移。
struct RegionVmo {
    vmo: Vmo,
    offset: u64,
}

// OWNER: 区域令牌表；令牌是下标，条目一经写入永不移除（区域在进程生命周期内保持映射）。
// 表自身是静态数组，不经过堆分配，避免分配器在持锁时递归进入分配器。
static REGIONS: Mutex<[Option<RegionVmo>; REGION_SLOTS]> =
    Mutex::new([const { None }; REGION_SLOTS]);

/// 映射 `size` 字节的新匿名内存，地址按 `align` 对齐；返回起点、所属 VMO 与起点在 VMO 内的偏移。
fn map(size: usize, align: usize) -> Option<(usize, Vmo, u64)> {
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
    Some((start, vmo, (start - base) as u64))
}

struct Native;

// SAFETY: 区域由 VMO 整段映射、读写、进程生命周期内不解除；decommit 只释放物理页，映射保持；
// futex 提供要求的等待/唤醒语义。
unsafe impl Backing for Native {
    fn reserve(&self, size: usize, align: usize) -> Option<(usize, usize)> {
        let (address, vmo, offset) = map(size, align)?;
        let mut regions = REGIONS.lock();
        let token = regions.iter().position(Option::is_none)?;
        regions[token] = Some(RegionVmo { vmo, offset });
        Some((address, token))
    }

    fn decommit(&self, token: usize, region_offset: usize, length: usize) {
        let regions = REGIONS.lock();
        if let Some(region) = &regions[token] {
            // 失败只意味着物理页暂不归还，内容与映射不受影响。
            let _ = region.vmo.op_range(
                native_abi::vmo::OP_DECOMMIT,
                region.offset + region_offset as u64,
                length as u64,
            );
        }
    }

    fn map_large(&self, size: usize, align: usize) -> Option<usize> {
        map(size, align).map(|(address, _, _)| address)
    }

    fn unmap_large(&self, address: usize, size: usize) {
        let size = size.next_multiple_of(native_abi::PAGE_SIZE);
        // SAFETY: 大块是独立映射，调用者保证不再使用。
        unsafe { vmar().unmap(address, size) }.expect("heap deallocation must retire its mapping");
    }

    fn thread_slot(&self) -> Option<&AtomicPtr<LocalHeap>> {
        crate::thread::heap_slot()
    }

    fn wait(&self, word: &AtomicU32, expected: u32) {
        // 值已变化或被打断时调用者重试。
        let _ = crate::futex::wait(word, expected, native_abi::DEADLINE_INFINITE, 0);
    }

    fn wake(&self, word: &AtomicU32) {
        crate::futex::wake(word, 1, 0);
    }
}

// OWNER: 进程唯一的堆状态；`Heap` 只是 `#[global_allocator]` 的零大小句柄，状态不随句柄复制。
static STATE: SpanHeap<Native> = SpanHeap::new(Native);

/// 注册为全局分配器的句柄。
pub(super) struct Heap;

/// 线程终止后交还其堆。
pub(super) fn retire_thread(slot: &AtomicPtr<LocalHeap>) {
    STATE.retire_thread(slot);
}

// SAFETY: 委托给满足 `GlobalAlloc` 契约的 span 堆。
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: 同 `GlobalAlloc::alloc`。
        unsafe { STATE.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: 同 `GlobalAlloc::dealloc`。
        unsafe { STATE.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: 同 `GlobalAlloc::realloc`。
        unsafe { STATE.realloc(pointer, layout, new_size) }
    }
}
