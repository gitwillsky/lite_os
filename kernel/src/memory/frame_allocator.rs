use alloc::vec::Vec;
use core::fmt::Debug;

use super::address::{PhysicalAddress, PhysicalPageNumber};
use spin::Once;

use crate::sync::IrqMutex;

// frame allocation 可由 global allocator 的 interrupt 路径到达，必须在取锁前关闭 local interrupt。
// OWNER: frame allocator module owns all allocatable physical-frame metadata.
static FRAME_ALLOCATOR: Once<IrqMutex<FrameAllocator>> = Once::new();

#[derive(Debug)]
enum FrameAllocError {
    OutOfRange,
    Duplicate,
}

/// @description 物理页请求是否允许消耗 kernel progress reserve。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameAllocationClass {
    /// 用户 residency、页表与可失败 kernel 工作；触及低水位时必须返回 OOM。
    Reclaimable,
    /// 普通 kernel heap extent；可进入 frame reserve，但必须保留 OOM cleanup 页。
    KernelHeap,
    /// 启动期 DMA；失败会阻止系统完成启动，允许越过最终 progress reserve。
    KernelCritical,
}

/// @description frame allocator 唯一 owner 的瞬时容量与碎片快照。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameStatistics {
    /// allocator 管辖的总页数。
    pub(crate) capacity_pages: usize,
    /// 所有 order 合计的当前空闲页数。
    pub(crate) free_pages: usize,
    /// 每个 order 的空闲 block 数；index n 表示 2ⁿ 页。
    pub(crate) free_blocks: [usize; usize::BITS as usize],
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

    /// @description 从已经撤销其他 owner publication 的物理 extent 重建唯一 RAII owner。
    /// @param ppn order-aligned extent 首个物理页号。
    /// @param pages 非零 2ⁿ 页数，frame allocator 中仍标记为 allocated。
    /// @return 不清零内容的唯一 FrameTracker。
    /// @safety caller 必须证明完整 extent 当前没有其他 owner、引用或 allocator membership。
    // SAFETY: caller must transfer one complete still-allocated frame extent with no aliases.
    pub(in crate::memory) unsafe fn from_raw(ppn: PhysicalPageNumber, pages: usize) -> Self {
        Self { ppn, pages }
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
        // 持有完整连续页范围，物理内存由 kernel direct map 覆盖且满足页对齐。
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

const ORDER_COUNT: usize = usize::BITS as usize;
const BLOCK_UNUSED: u8 = u8::MAX;
const BLOCK_ALLOCATED: u8 = 1 << 7;

#[derive(Clone, Copy)]
struct FreeBlockLinks {
    next: usize,
    previous: usize,
}

#[derive(Debug)]
struct FrameAllocator {
    start_ppn: PhysicalPageNumber,
    end_ppn: PhysicalPageNumber,
    // OWNER: order_heads/nonempty_orders/block_state 是同一 allocator lock 下的唯一
    // buddy metadata。heads 提供 O(1) pop/remove，bitmap 提供 O(1) 非空 order
    // 选择，block_state 以一 byte/page 证明 buddy 是同 order free block。缺任一
    // index 都会把 allocation/free 退化为全表扫描；三者禁止越过本 lock 单独更新。
    order_heads: [Option<PhysicalPageNumber>; ORDER_COUNT],
    nonempty_orders: usize,
    block_state: Vec<u8>,
    // free_blocks 是 order lists 的同锁计数 projection；缺失它时 procfs 为了
    // 观察碎片必须在 IRQ-off 内扫描所有 free block。
    free_blocks: [usize; ORDER_COUNT],
    // OWNER: free_pages 是上述 buddy metadata 的同锁 projection，只用于低水位
    // 快速判定。缺失同 transaction 加减会使 kernel reserve 被误放行或永久拒绝。
    free_pages: usize,
}

impl FrameAllocator {
    fn new(start_addr: PhysicalAddress, end_addr: PhysicalAddress) -> Self {
        let start = start_addr.ceil();
        let end = end_addr.floor();
        let capacity = end.as_usize() - start.as_usize();
        let mut block_state = Vec::new();
        block_state
            .try_reserve_exact(capacity)
            .expect("frame allocator metadata allocation failed");
        block_state.resize(capacity, BLOCK_UNUSED);
        let mut allocator = Self {
            start_ppn: start,
            end_ppn: end,
            order_heads: [None; ORDER_COUNT],
            nonempty_orders: 0,
            block_state,
            free_blocks: [0; ORDER_COUNT],
            free_pages: capacity,
        };

        // 将任意起点/长度区间分解为最大的 absolute-PPN-aligned buddy blocks。
        // 初始化只写每个 block 首页，成本与 block 数而非物理页数成正比。
        let mut cursor = start.as_usize();
        while cursor < end.as_usize() {
            let remaining = end.as_usize() - cursor;
            let alignment_order = cursor.trailing_zeros() as usize;
            let size_order = (usize::BITS - 1 - remaining.leading_zeros()) as usize;
            let order = alignment_order.min(size_order).min(ORDER_COUNT - 1);
            let block = PhysicalPageNumber::from(cursor);
            allocator.insert_free(block, order);
            cursor += 1usize << order;
        }
        allocator
    }

    fn capacity(&self) -> usize {
        self.end_ppn.as_usize() - self.start_ppn.as_usize()
    }

    fn state_index(&self, ppn: PhysicalPageNumber) -> Option<usize> {
        ppn.as_usize()
            .checked_sub(self.start_ppn.as_usize())
            .filter(|index| *index < self.block_state.len())
    }

    fn read_links(ppn: PhysicalPageNumber) -> FreeBlockLinks {
        // SAFETY: caller 只对已在 order free list 中的 block 读取；insert_free 在同一
        // allocator lock 下先写入完整 links，block 移出 list 前不会重新分配。
        unsafe { ppn.as_page_ptr().cast::<FreeBlockLinks>().read() }
    }

    fn write_links(ppn: PhysicalPageNumber, links: FreeBlockLinks) {
        // SAFETY: caller 持有 allocator lock 且 ppn 属于正在插入/更新的 free block；
        // free block 首页不再属于任何 FrameTracker，可独占存放 intrusive links。
        unsafe { ppn.as_page_mut_ptr().cast::<FreeBlockLinks>().write(links) };
    }

    fn insert_free(&mut self, block: PhysicalPageNumber, order: usize) {
        let index = self
            .state_index(block)
            .expect("free buddy block outside allocator range");
        assert_eq!(
            self.block_state[index], BLOCK_UNUSED,
            "free buddy block already has state"
        );
        let head = self.order_heads[order];
        Self::write_links(
            block,
            FreeBlockLinks {
                next: head.map_or(0, |page| page.as_usize()),
                previous: 0,
            },
        );
        if let Some(head) = head {
            let mut links = Self::read_links(head);
            assert_eq!(links.previous, 0, "free-list head has a predecessor");
            links.previous = block.as_usize();
            Self::write_links(head, links);
        }
        self.order_heads[order] = Some(block);
        self.nonempty_orders |= 1usize << order;
        self.block_state[index] = order as u8;
        self.free_blocks[order] = self.free_blocks[order]
            .checked_add(1)
            .expect("free buddy block count overflow");
    }

    fn remove_free(&mut self, block: PhysicalPageNumber, order: usize) {
        let index = self
            .state_index(block)
            .expect("removed buddy block outside allocator range");
        assert_eq!(
            self.block_state[index], order as u8,
            "removed buddy block has wrong order"
        );
        let links = Self::read_links(block);
        if links.previous == 0 {
            assert_eq!(self.order_heads[order], Some(block));
            self.order_heads[order] =
                (links.next != 0).then(|| PhysicalPageNumber::from(links.next));
        } else {
            let previous = PhysicalPageNumber::from(links.previous);
            let mut previous_links = Self::read_links(previous);
            previous_links.next = links.next;
            Self::write_links(previous, previous_links);
        }
        if links.next != 0 {
            let next = PhysicalPageNumber::from(links.next);
            let mut next_links = Self::read_links(next);
            next_links.previous = links.previous;
            Self::write_links(next, next_links);
        }
        if self.order_heads[order].is_none() {
            self.nonempty_orders &= !(1usize << order);
        }
        self.block_state[index] = BLOCK_UNUSED;
        self.free_blocks[order] = self.free_blocks[order]
            .checked_sub(1)
            .expect("free buddy block count underflow");
    }

    fn has_capacity(&self, pages: usize, class: FrameAllocationClass) -> bool {
        let reserve = match class {
            FrameAllocationClass::Reclaimable => super::KERNEL_HEAP_GROWTH_PAGES,
            FrameAllocationClass::KernelHeap => super::KERNEL_PROGRESS_RESERVE_PAGES,
            FrameAllocationClass::KernelCritical => 0,
        };
        self.free_pages
            .checked_sub(pages)
            .is_some_and(|remaining| remaining >= reserve)
    }

    fn alloc_order(
        &mut self,
        requested_order: usize,
        class: FrameAllocationClass,
    ) -> Option<PhysicalPageNumber> {
        if requested_order >= ORDER_COUNT {
            return None;
        }
        let pages = 1usize << requested_order;
        if !self.has_capacity(pages, class) {
            return None;
        }
        let available = self.nonempty_orders & (!0usize << requested_order);
        if available == 0 {
            return None;
        }
        let mut order = available.trailing_zeros() as usize;
        let block = self.order_heads[order].expect("nonempty order lost its head");
        self.remove_free(block, order);

        // 只把右半 block 插回低 order list，左半保持同一起点继续拆分。
        // 快路径无全表扫描，IRQ-off 工作上限为 ORDER_COUNT。
        while order > requested_order {
            order -= 1;
            let right = PhysicalPageNumber::from(block.as_usize() + (1usize << order));
            self.insert_free(right, order);
        }
        let index = self
            .state_index(block)
            .expect("allocated buddy block outside allocator range");
        assert_eq!(self.block_state[index], BLOCK_UNUSED);
        self.block_state[index] = BLOCK_ALLOCATED | requested_order as u8;
        self.free_pages -= pages;
        Some(block)
    }

    fn alloc(&mut self, class: FrameAllocationClass) -> Option<PhysicalPageNumber> {
        self.alloc_order(0, class)
    }

    fn alloc_contiguous(
        &mut self,
        pages: usize,
        class: FrameAllocationClass,
    ) -> Option<(PhysicalPageNumber, usize)> {
        if pages == 0 {
            return None;
        }
        let allocated_pages = pages.checked_next_power_of_two()?;
        let order = allocated_pages.trailing_zeros() as usize;
        self.alloc_order(order, class)
            .map(|block| (block, allocated_pages))
    }

    fn dealloc_contiguous(
        &mut self,
        start: PhysicalPageNumber,
        pages: usize,
    ) -> Result<(), FrameAllocError> {
        let Some(end) = start.as_usize().checked_add(pages) else {
            return Err(FrameAllocError::OutOfRange);
        };
        if pages == 0
            || !pages.is_power_of_two()
            || !start.as_usize().is_multiple_of(pages)
            || start < self.start_ppn
            || end > self.end_ppn.as_usize()
        {
            return Err(FrameAllocError::OutOfRange);
        }
        let original_order = pages.trailing_zeros() as usize;
        let index = self.state_index(start).ok_or(FrameAllocError::OutOfRange)?;
        if self.block_state[index] != BLOCK_ALLOCATED | original_order as u8 {
            return Err(FrameAllocError::Duplicate);
        }
        self.block_state[index] = BLOCK_UNUSED;
        self.free_pages = self
            .free_pages
            .checked_add(pages)
            .filter(|free| *free <= self.capacity())
            .expect("free frame count exceeds allocator capacity");

        // 每层只通过一 byte state 定位 buddy，双链 remove 为 O(1)；释放大
        // FrameTracker 不再按页写链，IRQ-off 时间只与最终 order 成正比。
        let mut block = start;
        let mut order = original_order;
        while order + 1 < ORDER_COUNT {
            let buddy = PhysicalPageNumber::from(block.as_usize() ^ (1usize << order));
            let Some(buddy_index) = self.state_index(buddy) else {
                break;
            };
            if self.block_state[buddy_index] != order as u8 {
                break;
            }
            self.remove_free(buddy, order);
            block = block.min(buddy);
            order += 1;
        }
        self.insert_free(block, order);
        Ok(())
    }

    fn statistics(&self) -> FrameStatistics {
        FrameStatistics {
            capacity_pages: self.capacity(),
            free_pages: self.free_pages,
            free_blocks: self.free_blocks,
        }
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
    let res = FRAME_ALLOCATOR
        .wait()
        .lock()
        .alloc(FrameAllocationClass::Reclaimable);
    res.map(FrameTracker::new)
}

fn alloc_unzeroed_raw() -> Option<FrameTracker> {
    FRAME_ALLOCATOR
        .wait()
        .lock()
        .alloc(FrameAllocationClass::Reclaimable)
        .map(|ppn| FrameTracker { ppn, pages: 1 })
}

/// @description 从唯一 frame allocator 分配一页；触及 kernel progress 低水位时回收后重试。
pub(crate) fn alloc() -> Option<FrameTracker> {
    if let Some(frame) = alloc_raw() {
        return Some(frame);
    }
    let _ = super::shared_file::reclaim_pages(64);
    alloc_raw()
}

/// @description 分配一页并在 publication 前用完整 source page 覆盖其旧内容。
/// @param source 必须恰好为一页；仅供 COW 等完整覆盖路径使用。
/// @return 成功返回不经过 zero-fill、但已完全初始化的唯一 FrameTracker。
/// @errors 内存回收后仍无空闲页时返回 None；长度不是一页表示 caller 破坏安全契约并 fail-stop。
pub(crate) fn alloc_copy(source: &[u8]) -> Option<FrameTracker> {
    assert_eq!(
        source.len(),
        super::config::PAGE_SIZE,
        "full-overwrite frame source must be exactly one page"
    );
    let mut frame = alloc_unzeroed_raw().or_else(|| {
        let _ = super::shared_file::reclaim_pages(64);
        alloc_unzeroed_raw()
    })?;
    // OWNER: frame 尚未进入 page table、Arc 或 allocator free list；完整 copy 是唯一
    // publication 前初始化。若改成 partial copy，旧进程数据会暴露给新映射。
    frame.bytes_mut().copy_from_slice(source);
    Some(frame)
}

/// @description 分配并清零指定数量的连续物理页。
///
/// @param pages 非零页数。
/// @param class 是否允许消耗 kernel progress reserve。
/// @return 成功返回唯一 `FrameTracker`，实际页数向上取整为 2ⁿ 以保证
/// 同尺寸对齐；回收后仍无该 order 区间返回 `None`。
pub(crate) fn alloc_contiguous(pages: usize, class: FrameAllocationClass) -> Option<FrameTracker> {
    let mut tracker = alloc_contiguous_uninitialized(pages, class)?;
    tracker.bytes_mut().fill(0);
    Some(tracker)
}

/// @description 为 kernel global allocator 分配不做 dead zero-fill 的连续 extent。
///
/// @param pages 非零页数；实际页数按 buddy order 向上取整。
/// @return 成功返回尚未发布、内容不可读的唯一 extent owner。
/// @errors 回收后仍没有可用 `KernelHeap` extent 时返回 `None`。
///
/// Rust allocator 的成功分配结果本来就是 uninitialized storage；只有 heap owner
/// 可以调用本 seam。若把它用于 user mapping、DMA read buffer 或任何 partial-init
/// publication，旧物理页内容会被观察到。
pub(in crate::memory) fn alloc_heap_extent(pages: usize) -> Option<FrameTracker> {
    alloc_contiguous_uninitialized(pages, FrameAllocationClass::KernelHeap)
}

fn alloc_contiguous_uninitialized(
    pages: usize,
    class: FrameAllocationClass,
) -> Option<FrameTracker> {
    if pages == 0 {
        return None;
    }
    let allocation_pages = pages.checked_next_power_of_two()?;
    let mut res = FRAME_ALLOCATOR.wait().lock().alloc_contiguous(pages, class);
    if res.is_none() {
        let _ = super::shared_file::reclaim_pages(allocation_pages.max(64));
        res = FRAME_ALLOCATOR.wait().lock().alloc_contiguous(pages, class);
    }
    res.map(|(ppn, pages)| FrameTracker { ppn, pages })
}

/// @description 返回 frame allocator 管辖范围的总页数与当前空闲页数。
///
/// @return 容量、空闲页和每 order block 数；均来自唯一 allocator 状态。
pub(crate) fn statistics() -> FrameStatistics {
    FRAME_ALLOCATOR.wait().lock().statistics()
}
