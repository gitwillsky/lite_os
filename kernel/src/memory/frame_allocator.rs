use core::fmt::Debug;

use super::address::{PhysicalAddress, PhysicalPageNumber};
use spin::Once;

use crate::sync::IrqMutex;

// frame allocation 可由 global allocator 的 interrupt 路径到达，必须在取锁前关闭本地 SIE。
// OWNER: frame allocator module owns all allocatable physical-frame metadata.
static FRAME_ALLOCATOR: Once<IrqMutex<StackFrameAllocator>> = Once::new();

#[derive(Debug)]
enum FrameAllocError {
    OutOfRange,
    Duplicate,
}

pub(crate) struct FrameTracker {
    pub(crate) ppn: PhysicalPageNumber,
    pub(crate) pages: usize, // Number of pages (1 for single page, >1 for contiguous allocation)
}

impl FrameTracker {
    fn new(ppn: PhysicalPageNumber) -> Self {
        let mut tracker = Self { ppn, pages: 1 };
        tracker.bytes_mut().fill(0);
        tracker
    }

    fn new_contiguous(ppn: PhysicalPageNumber, pages: usize) -> Self {
        let mut tracker = Self { ppn, pages };
        tracker.bytes_mut().fill(0);
        tracker
    }

    /// @description 独占借用 tracker 拥有的连续物理页内容。
    ///
    /// @return 生命周期绑定到 tracker 独占借用的可写字节切片。
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self
            .pages
            .checked_mul(super::config::PAGE_SIZE)
            .expect("frame byte length overflow");
        // SAFETY: &mut FrameTracker 保证本 tracker 的 Rust 访问独占；tracker 在借用期间
        // 持有完整连续页范围，物理内存由 kernel identity mapping 覆盖且满足页对齐。
        unsafe { core::slice::from_raw_parts_mut(self.ppn.as_page_mut_ptr(), len) }
    }

    /// @description 只读借用 tracker 拥有的连续物理页内容。
    ///
    /// @return 生命周期绑定到 tracker 的只读字节切片。
    pub(crate) fn bytes(&self) -> &[u8] {
        let len = self
            .pages
            .checked_mul(super::config::PAGE_SIZE)
            .expect("frame byte length overflow");
        // SAFETY: FrameTracker 在借用期间持有完整物理页范围；只返回共享只读切片。
        unsafe { core::slice::from_raw_parts(self.ppn.as_page_ptr(), len) }
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        // For contiguous pages, deallocate each page individually
        for i in 0..self.pages {
            let current_ppn = PhysicalPageNumber::from(self.ppn.as_usize() + i);
            if let Err(error) = FRAME_ALLOCATOR.wait().lock().dealloc(current_ppn) {
                panic!(
                    "invalid FrameTracker drop for {:?}: {:?}",
                    current_ppn, error
                );
            }
        }
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_fmt(format_args!(
            "FrameTracker PPN:{:#x} pages:{}",
            self.ppn.as_usize(),
            self.pages
        ))
    }
}

#[derive(Debug)]
struct StackFrameAllocator {
    start_ppn: PhysicalPageNumber,
    current_start_ppn: PhysicalPageNumber,
    end_ppn: PhysicalPageNumber,

    recycled_head: Option<PhysicalPageNumber>,
    recycled_len: usize,
}

impl StackFrameAllocator {
    pub(crate) fn new(start_addr: PhysicalAddress, end_addr: PhysicalAddress) -> Self {
        let start = start_addr.ceil();
        let end = end_addr.floor();
        Self {
            start_ppn: start,
            current_start_ppn: start,
            end_ppn: end,
            recycled_head: None,
            recycled_len: 0,
        }
    }

    pub(crate) fn alloc(&mut self) -> Option<PhysicalPageNumber> {
        if let Some(ppn) = self.recycled_head {
            self.recycled_head = Self::recycled_next(ppn);
            self.recycled_len -= 1;
            // Never return PPN 0 from recycled pages
            if ppn.as_usize() == 0 {
                panic!("Frame allocator recycled PPN 0, this should never happen");
            }
            Some(ppn)
        } else if self.current_start_ppn < self.end_ppn {
            let current = self.current_start_ppn;
            // Never return PPN 0 from allocation
            if current.as_usize() == 0 {
                panic!("Frame allocator current_start_ppn is 0, this should never happen");
            }
            self.current_start_ppn = current.add_one();
            Some(current)
        } else {
            None
        }
    }

    pub(crate) fn alloc_contiguous(&mut self, pages: usize) -> Option<PhysicalPageNumber> {
        if pages == 0 {
            return None;
        }

        // 1. 优先从从未分配的尾部取得连续区间，保持常见路径 O(1)。
        let allocation_end = self.current_start_ppn.as_usize().checked_add(pages)?;
        if allocation_end <= self.end_ppn.as_usize() {
            let start_ppn = self.current_start_ppn;
            // Never return PPN 0 from contiguous allocation
            if start_ppn.as_usize() == 0 {
                panic!("Frame allocator current_start_ppn is 0, this should never happen");
            }
            self.current_start_ppn = PhysicalPageNumber::from(allocation_end);
            Some(start_ppn)
        } else {
            // 2. 尾部不足时扫描 recycled intrusive list；缺少此路径会让动态 heap 在
            //    物理页总量充足但发生回收后仍因假性连续内存耗尽而失败。
            let mut candidate = self.recycled_head;
            while let Some(start) = candidate {
                let start_value = start.as_usize();
                let complete = (0..pages).all(|offset| {
                    start_value
                        .checked_add(offset)
                        .filter(|page| *page < self.end_ppn.as_usize())
                        .is_some_and(|page| self.recycled_contains(PhysicalPageNumber::from(page)))
                });
                if complete {
                    for offset in 0..pages {
                        self.remove_recycled(PhysicalPageNumber::from(
                            start_value
                                .checked_add(offset)
                                .expect("validated contiguous recycled run"),
                        ));
                    }
                    return Some(start);
                }
                candidate = Self::recycled_next(start);
            }
            None
        }
    }

    fn recycled_contains(&self, target: PhysicalPageNumber) -> bool {
        let mut cursor = self.recycled_head;
        while let Some(page) = cursor {
            if page == target {
                return true;
            }
            cursor = Self::recycled_next(page);
        }
        false
    }

    fn remove_recycled(&mut self, target: PhysicalPageNumber) {
        let mut previous: Option<PhysicalPageNumber> = None;
        let mut cursor = self.recycled_head;
        while let Some(page) = cursor {
            let next = Self::recycled_next(page);
            if page == target {
                if let Some(previous) = previous {
                    // SAFETY: previous 仍在 recycled list 中，其页首 next 字段由 allocator lock 独占。
                    unsafe {
                        previous
                            .as_page_mut_ptr()
                            .cast::<usize>()
                            .write(next.map_or(0, |page| page.as_usize()))
                    };
                } else {
                    self.recycled_head = next;
                }
                self.recycled_len -= 1;
                return;
            }
            previous = Some(page);
            cursor = next;
        }
        panic!("contiguous recycled run changed while allocator lock is held");
    }

    pub(crate) fn dealloc(&mut self, ppn: PhysicalPageNumber) -> Result<(), FrameAllocError> {
        // 验证 PPN 在有效范围内
        if ppn < self.start_ppn || ppn >= self.end_ppn {
            return Err(FrameAllocError::OutOfRange);
        }

        // 检查是否试图释放未分配的页面
        // 如果 ppn >= current_start_ppn，说明这个页面还没有被分配过
        if ppn >= self.current_start_ppn {
            return Err(FrameAllocError::OutOfRange);
        }

        // 检查重复释放 - 这是一个原子操作在单线程环境下
        let mut cursor = self.recycled_head;
        while let Some(recycled) = cursor {
            if recycled == ppn {
                return Err(FrameAllocError::Duplicate);
            }
            cursor = Self::recycled_next(recycled);
        }

        let next = self.recycled_head.map_or(0, |head| head.as_usize());
        // SAFETY: dealloc 的范围/分配状态检查证明 ppn 是不再被 FrameTracker 拥有的完整页；
        // frame lock 保证 intrusive free-list 只有一个写者，页首一个 usize 用作 next PPN。
        unsafe { ppn.as_page_mut_ptr().cast::<usize>().write(next) };
        self.recycled_head = Some(ppn);
        self.recycled_len += 1;
        Ok(())
    }

    fn recycled_next(ppn: PhysicalPageNumber) -> Option<PhysicalPageNumber> {
        // SAFETY: 只有 recycled_head 可达页会进入本函数；这些页由 dealloc 在相同 frame
        // lock 下写入 next PPN，且在从链表移除前不会重新分配。
        let next = unsafe { ppn.as_page_ptr().cast::<usize>().read() };
        (next != 0).then(|| PhysicalPageNumber::from(next))
    }

    fn capacity_and_free_pages(&self) -> (usize, usize) {
        let capacity = self.end_ppn.as_usize() - self.start_ppn.as_usize();
        let never_allocated = self.end_ppn.as_usize() - self.current_start_ppn.as_usize();
        (capacity, never_allocated + self.recycled_len)
    }
}

pub(crate) fn init(start_addr: PhysicalAddress, end_addr: PhysicalAddress) {
    debug!(
        "frame_allocator::init: start_addr={:#x}, end_addr={:#x}",
        start_addr.as_usize(),
        end_addr.as_usize()
    );

    let start_ppn = start_addr.ceil();
    let end_ppn = end_addr.floor();

    assert!(
        end_ppn.as_usize() > start_ppn.as_usize(),
        "frame_allocator: range is 0, start_ppn={:#x}, end_ppn={:#x}",
        start_ppn.as_usize(),
        end_ppn.as_usize()
    );

    // 验证PPN的合理性
    if start_ppn.as_usize() == 0 {
        panic!(
            "Invalid start PPN: zero page number from address {:#x}",
            start_addr.as_usize()
        );
    }

    FRAME_ALLOCATOR.call_once(|| IrqMutex::new(StackFrameAllocator::new(start_addr, end_addr)));
}

fn alloc_raw() -> Option<FrameTracker> {
    let res = FRAME_ALLOCATOR.wait().lock().alloc();
    res.map(FrameTracker::new)
}

/// @description 从唯一 frame allocator 分配一页；耗尽时统一回收可重建用户页后重试。
pub(crate) fn alloc() -> Option<FrameTracker> {
    if let Some(frame) = alloc_raw() {
        return Some(frame);
    }
    super::shared_file::reclaim_pages(64);
    alloc_raw()
}

pub(crate) fn alloc_contiguous(pages: usize) -> Option<FrameTracker> {
    let mut res = FRAME_ALLOCATOR.wait().lock().alloc_contiguous(pages);
    if res.is_none() {
        super::shared_file::reclaim_pages(pages.max(64));
        res = FRAME_ALLOCATOR.wait().lock().alloc_contiguous(pages);
    }
    res.map(|b| FrameTracker::new_contiguous(b, pages))
}

/// @description 返回 frame allocator 管辖范围的总页数与当前空闲页数。
///
/// @return `(capacity_pages, free_pages)`；两者均来自唯一 allocator 状态。
pub(crate) fn statistics() -> (usize, usize) {
    FRAME_ALLOCATOR.wait().lock().capacity_and_free_pages()
}
