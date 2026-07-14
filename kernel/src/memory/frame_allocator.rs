use core::fmt::Debug;

use super::address::{PhysicalAddress, PhysicalPageNumber};
use spin::Once;

use crate::sync::IrqMutex;

// frame allocation 可由 global allocator 的 interrupt 路径到达，必须在取锁前关闭本地 SIE。
// OWNER: frame allocator module owns all allocatable physical-frame metadata.
static FRAME_ALLOCATOR: Once<IrqMutex<FrameAllocator>> = Once::new();

#[derive(Debug)]
enum FrameAllocError {
    OutOfRange,
    Duplicate,
}

/// @description 一个或多个连续物理页的唯一 RAII owner。
pub(crate) struct FrameTracker {
    /// 连续区间的首个物理页号。
    pub(crate) ppn: PhysicalPageNumber,
    /// 区间页数；始终非零。
    pub(crate) pages: usize,
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
        if let Err(error) = FRAME_ALLOCATOR
            .wait()
            .lock()
            .dealloc_contiguous(self.ppn, self.pages)
        {
            panic!(
                "invalid FrameTracker drop for {:?}+{} pages: {:?}",
                self.ppn, self.pages, error
            );
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
struct FrameAllocator {
    start_ppn: PhysicalPageNumber,
    current_start_ppn: PhysicalPageNumber,
    end_ppn: PhysicalPageNumber,

    // OWNER: allocator lock 内的 recycled pages 以页首 usize 组成严格递增 intrusive list。
    // 缺失顺序会让 contiguous fallback 反复全表 membership/remove，最坏退化为立方扫描。
    recycled_head: Option<PhysicalPageNumber>,
    recycled_len: usize,
}

impl FrameAllocator {
    fn new(start_addr: PhysicalAddress, end_addr: PhysicalAddress) -> Self {
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

    fn alloc(&mut self) -> Option<PhysicalPageNumber> {
        if let Some(ppn) = self.recycled_head {
            self.recycled_head = Self::recycled_next(ppn);
            self.recycled_len -= 1;
            Some(ppn)
        } else if self.current_start_ppn < self.end_ppn {
            let current = self.current_start_ppn;
            self.current_start_ppn = current.add_one();
            Some(current)
        } else {
            None
        }
    }

    fn alloc_contiguous(&mut self, pages: usize) -> Option<PhysicalPageNumber> {
        if pages == 0 {
            return None;
        }

        // 1. 优先从从未分配的尾部取得连续区间，保持常见路径 O(1)。
        let allocation_end = self.current_start_ppn.as_usize().checked_add(pages)?;
        if allocation_end <= self.end_ppn.as_usize() {
            let start_ppn = self.current_start_ppn;
            self.current_start_ppn = PhysicalPageNumber::from(allocation_end);
            Some(start_ppn)
        } else {
            // 2. 有序 intrusive list 让连续 run 在链上相邻；单次扫描同时定位 run 与
            // 前驱，找到后 O(1) splice。若恢复无序 list，此处会退化为重复全表查找。
            if self.recycled_len < pages {
                return None;
            }
            let mut cursor = self.recycled_head;
            let mut previous = None;
            let mut run_start = None;
            let mut run_before = None;
            let mut run_len = 0;
            while let Some(page) = cursor {
                let next = Self::recycled_next(page);
                if previous.is_some_and(|previous: PhysicalPageNumber| {
                    previous.as_usize().checked_add(1) == Some(page.as_usize())
                }) {
                    run_len += 1;
                } else {
                    run_start = Some(page);
                    run_before = previous;
                    run_len = 1;
                }
                if run_len == pages {
                    let start = run_start.expect("non-empty recycled run lost its start");
                    if let Some(before) = run_before {
                        Self::set_recycled_next(before, next);
                    } else {
                        self.recycled_head = next;
                    }
                    self.recycled_len -= pages;
                    return Some(start);
                }
                previous = Some(page);
                cursor = next;
            }
            None
        }
    }

    fn dealloc_contiguous(
        &mut self,
        start: PhysicalPageNumber,
        pages: usize,
    ) -> Result<(), FrameAllocError> {
        let Some(end) = start.as_usize().checked_add(pages) else {
            return Err(FrameAllocError::OutOfRange);
        };
        // 1. 整段必须属于 allocator 已发布区间；先完成验证，失败时 recycler 保持不变。
        if pages == 0
            || start < self.start_ppn
            || end > self.current_start_ppn.as_usize()
            || end > self.end_ppn.as_usize()
        {
            return Err(FrameAllocError::OutOfRange);
        }

        // 2. 有序 list 一次定位插入点；首个 >= start 的节点落在 end 前即证明区间重叠。
        let mut previous = None;
        let mut cursor = self.recycled_head;
        while let Some(page) = cursor {
            if page >= start {
                break;
            }
            previous = Some(page);
            cursor = Self::recycled_next(page);
        }
        if cursor.is_some_and(|page| page.as_usize() < end) {
            return Err(FrameAllocError::Duplicate);
        }
        let recycled_len = self
            .recycled_len
            .checked_add(pages)
            .filter(|length| {
                *length <= self.current_start_ppn.as_usize() - self.start_ppn.as_usize()
            })
            .expect("recycled frame count exceeds allocated range");

        // 3. 一次 allocator transaction 串起整段页，并把它 splice 到唯一排序位置。
        for offset in 0..pages {
            let page = PhysicalPageNumber::from(start.as_usize() + offset);
            let next = if offset + 1 == pages {
                cursor
            } else {
                Some(PhysicalPageNumber::from(page.as_usize() + 1))
            };
            Self::set_recycled_next(page, next);
        }
        if let Some(previous) = previous {
            Self::set_recycled_next(previous, Some(start));
        } else {
            self.recycled_head = Some(start);
        }
        self.recycled_len = recycled_len;
        Ok(())
    }

    fn set_recycled_next(ppn: PhysicalPageNumber, next: Option<PhysicalPageNumber>) {
        debug_assert!(next.is_none_or(|next| next > ppn));
        // SAFETY: caller 持有唯一 allocator lock，且 ppn 是已在 recycler 中或正由
        // FrameTracker Drop 交还的完整页；页首 usize 在重新分配前只存放 next PPN。
        unsafe {
            ppn.as_page_mut_ptr()
                .cast::<usize>()
                .write(next.map_or(0, |page| page.as_usize()))
        };
    }

    fn recycled_next(ppn: PhysicalPageNumber) -> Option<PhysicalPageNumber> {
        // SAFETY: 只有 recycled_head 可达页会进入本函数；这些页由 dealloc_contiguous 在
        // 相同 allocator lock 下写入 next PPN，且在从链表移除前不会重新分配。
        let next = unsafe { ppn.as_page_ptr().cast::<usize>().read() };
        (next != 0).then(|| PhysicalPageNumber::from(next))
    }

    fn capacity_and_free_pages(&self) -> (usize, usize) {
        let capacity = self.end_ppn.as_usize() - self.start_ppn.as_usize();
        let never_allocated = self.end_ppn.as_usize() - self.current_start_ppn.as_usize();
        (capacity, never_allocated + self.recycled_len)
    }
}

/// @description 发布覆盖给定物理区间的唯一 frame allocator。
///
/// @param start_addr allocator 可用区间起点。
/// @param end_addr allocator 可用区间 exclusive end。
/// @return 无返回值。
/// @errors 空区间、零页或重复初始化时 fail-stop。
pub(crate) fn init(start_addr: PhysicalAddress, end_addr: PhysicalAddress) {
    assert!(
        FRAME_ALLOCATOR.get().is_none(),
        "frame allocator initialized twice"
    );
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

    FRAME_ALLOCATOR.call_once(|| IrqMutex::new(FrameAllocator::new(start_addr, end_addr)));
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

/// @description 分配并清零指定数量的连续物理页。
///
/// @param pages 非零页数。
/// @return 成功返回唯一 `FrameTracker`；回收后仍无连续区间返回 `None`。
pub(crate) fn alloc_contiguous(pages: usize) -> Option<FrameTracker> {
    if pages == 0 {
        return None;
    }
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
