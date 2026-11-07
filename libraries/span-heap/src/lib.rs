#![cfg_attr(not(any(test, feature = "host")), no_std)]

//! 线程本地分片的 span 堆：mimalloc 的分片空闲链（Leijen et al., APLAS 2019）与 snmalloc 式的远端释放消息
//! （Lietar et al., ISMM 2019），为没有 TLS、没有 mmap 的系统服务运行时定制。
//!
//! # 结构
//!
//! 1. **区域（region）**：64 MiB 虚拟区间，一次预留、按需提交。区域头部 2 MiB 放元数据（每 64 KiB 单元一个
//!    [`SpanMeta`]），其余切成 64 KiB 单元。span 是 2 的幂个单元（64 KiB–2 MiB），按自身大小对齐。
//! 2. **span**：服务一个尺寸级别（见 [`classes`]）。块只在被分配时才由 `bump` 游标切出（增量空闲链），
//!    新 span 不会提前触碰整块页面。
//! 3. **线程本地堆**：每个线程一个 [`LocalHeap`]，每级别一个 active span 与一条 partial 链。分配与本线程释放
//!    不加锁、不用原子读-改-写。
//! 4. **远端释放**：非所属线程释放时把块压入 span 的原子栈，并在 span 首次有待收集块时把它压入所属堆的
//!    `pending` 栈；所属线程在慢路径整体收走（单消费者，无 ABA）。
//! 5. **线程退出**：所属堆的 span 转为孤儿，由其他线程在慢路径认领；空 span 立即归还。
//! 6. **归还**：空 span 回到全局按阶栈；每阶最多保留 [`KEEP_COMMITTED`] 个已提交的，其余先 decommit
//!    （不 unmap，地址空间保持），所以空闲服务不保留历史峰值。
//! 7. 大于 [`classes::MAX_SMALL`] 的请求使用独立映射。
//!
//! # 并发与所有权
//!
//! 全局锁只保护 span 的分配/归还、孤儿链与堆池，不在热路径上。SpanMeta 的非原子字段只由所属堆（或持全局锁者）
//! 访问；`remote`、`owner`、`queued`、`pending_next` 是跨线程协议字段。

pub mod classes;
#[cfg(any(test, feature = "host"))]
pub mod host;

use classes::{CLASS_COUNT, MAX_SMALL, MIN_BLOCK, block_size, class_of};
use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

const UNIT_SHIFT: usize = 16;
const UNIT: usize = 1 << UNIT_SHIFT;
/// span 的最大阶数：2 MiB。
const MAX_ORDER: usize = 5;
const REGION_SHIFT: usize = 26;
const REGION_SIZE: usize = 1 << REGION_SHIFT;
const UNITS_PER_REGION: usize = REGION_SIZE / UNIT;
/// 区域头部留给元数据的单元数（2 MiB，保证数据区对最大 span 对齐）。
const META_UNITS: usize = 1 << MAX_ORDER;
const MAX_REGIONS: usize = 256;
const TABLE_SIZE: usize = 1024;
/// 每阶保留的已提交空闲 span 数；其余 decommit。
const KEEP_COMMITTED: u32 = 4;
/// 堆池一次预留的字节数。
const HEAP_CHUNK: usize = 64 * 1024;

/// 底层内存与同步由嵌入者提供。
///
/// # Safety
///
/// `reserve` 返回的地址区间必须可读写、归调用者独占且在进程生命周期内保持映射；`decommit` 之后该区间读为零或
/// 保留原内容均可，但必须仍可读写；`wait`/`wake` 必须满足 futex 语义。
pub unsafe trait Backing: Sync {
    /// 预留 `size` 字节可读写、按需提交的内存，地址按 `align` 对齐。返回基址与供 `decommit` 使用的令牌。
    fn reserve(&self, size: usize, align: usize) -> Option<(usize, usize)>;
    /// 释放 `[region_offset, +length)` 的物理页，区间保持映射。
    fn decommit(&self, token: usize, region_offset: usize, length: usize);
    /// 独立映射大对象。
    fn map_large(&self, size: usize, align: usize) -> Option<usize>;
    /// 解除 [`Self::map_large`] 的映射。
    fn unmap_large(&self, address: usize, size: usize);
    /// 当前线程保存其堆指针的槽；运行时尚未安装线程描述符时为 `None`（使用带锁的后备堆）。
    fn thread_slot(&self) -> Option<&AtomicPtr<LocalHeap>>;
    /// 在 `word` 仍等于 `expected` 时睡眠。
    fn wait(&self, word: &AtomicU32, expected: u32);
    /// 唤醒一个 `word` 上的等待者。
    fn wake(&self, word: &AtomicU32);
}

#[repr(C)]
struct Block {
    next: *mut Block,
}

/// 一个 span 的元数据，位于所属区域头部。全零是合法的“未使用”状态。
#[repr(C)]
struct SpanMeta {
    // 以下字段只由所属堆（或持全局锁者）访问。
    free: *mut Block,
    /// partial 链或全局链（空闲 span 栈、孤儿链）的前后向。
    prev: *mut SpanMeta,
    next: *mut SpanMeta,
    owned_prev: *mut SpanMeta,
    owned_next: *mut SpanMeta,
    base: usize,
    block_size: u32,
    used: u32,
    bump: u32,
    limit: u32,
    class: u8,
    order: u8,
    /// 在所属堆的 partial 链上。缺失会让同一 span 被重复挂链。
    listed: bool,
    /// 页面可能已提交（曾被分配或写入）；用于决定归还时是否需要 decommit。
    committed: bool,
    // 以下字段是跨线程协议。
    owner: AtomicPtr<LocalHeap>,
    remote: AtomicPtr<Block>,
    /// 当前持有本 span 的 pending 栈所属的堆（空表示不在任何栈上）；CAS 保证 span 同时最多在一个栈上，
    /// `pending_next` 因此不会被两个栈同时使用。
    queued: AtomicPtr<LocalHeap>,
    pending_next: AtomicPtr<SpanMeta>,
}

#[repr(C)]
struct RegionMeta {
    token: usize,
    /// 每个单元所属 span 起点的单元号，让块指针 O(1) 找到 SpanMeta。
    start_of: [u16; UNITS_PER_REGION],
    spans: [SpanMeta; UNITS_PER_REGION],
}

/// 线程本地堆；对外只作为不透明指针保存在线程槽中。
#[repr(C)]
pub struct LocalHeap {
    active: [*mut SpanMeta; CLASS_COUNT],
    partial: [*mut SpanMeta; CLASS_COUNT],
    owned: *mut SpanMeta,
    pending: AtomicPtr<SpanMeta>,
    next_pooled: *mut LocalHeap,
}

struct Lock(AtomicU32);

impl Lock {
    const fn new() -> Self {
        Self(AtomicU32::new(0))
    }

    fn lock<'a, B: Backing>(&'a self, backing: &'a B) -> LockGuard<'a, B> {
        if self
            .0
            .compare_exchange(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // 置 2 后再睡：解锁者看到 2 一定唤醒。
            while self.0.swap(2, Ordering::Acquire) != 0 {
                backing.wait(&self.0, 2);
            }
        }
        LockGuard {
            lock: self,
            backing,
        }
    }
}

struct LockGuard<'a, B: Backing> {
    lock: &'a Lock,
    backing: &'a B,
}

impl<B: Backing> Drop for LockGuard<'_, B> {
    fn drop(&mut self) {
        if self.lock.0.swap(0, Ordering::Release) == 2 {
            self.backing.wake(&self.lock.0);
        }
    }
}

/// 全局状态；只在持 `lock` 时修改（`table` 条目带 Release 发布，读取无锁）。
struct Shared {
    lock: Lock,
    regions: UnsafeCell<[usize; MAX_REGIONS]>,
    region_count: UnsafeCell<usize>,
    /// 当前区域内下一个未切出的单元。
    bump_unit: UnsafeCell<usize>,
    free_spans: UnsafeCell<[*mut SpanMeta; MAX_ORDER + 1]>,
    free_committed: UnsafeCell<[u32; MAX_ORDER + 1]>,
    orphans: UnsafeCell<[*mut SpanMeta; CLASS_COUNT]>,
    heap_pool: UnsafeCell<*mut LocalHeap>,
    heap_chunk: UnsafeCell<(usize, usize)>,
    /// 区域基址 `>> REGION_SHIFT` 的开放寻址表（存基址，0 为空）；区域永不移除。
    table: [AtomicUsize; TABLE_SIZE],
}

/// 带锁的后备堆：运行时安装线程描述符之前的分配走这里。
struct Fallback {
    lock: Lock,
    heap: UnsafeCell<LocalHeap>,
}

/// span 堆。
pub struct Heap<B: Backing> {
    backing: B,
    shared: Shared,
    fallback: Fallback,
}

// SAFETY: UnsafeCell 字段只在 `lock` 下访问；元数据字段的跨线程协议见模块文档；`backing` 本身是 Sync。
unsafe impl<B: Backing> Sync for Heap<B> {}
// SAFETY: 同上；堆里的裸指针都指向进程生命周期内映射的区域元数据与堆池，不绑定任何线程。
unsafe impl<B: Backing> Send for Heap<B> {}

impl LocalHeap {
    /// 空堆；作为函数而非常量，避免内部可变常量被按值复制成多个独立实例。
    const fn empty() -> Self {
        Self {
            active: [null_mut(); CLASS_COUNT],
            partial: [null_mut(); CLASS_COUNT],
            owned: null_mut(),
            pending: AtomicPtr::new(null_mut()),
            next_pooled: null_mut(),
        }
    }
}

fn span_order(class: usize) -> usize {
    let bytes = (block_size(class) * 8).next_power_of_two().max(UNIT);
    ((bytes >> UNIT_SHIFT).trailing_zeros() as usize).min(MAX_ORDER)
}

impl<B: Backing> Heap<B> {
    /// 创建空堆；可放入 `static`。
    pub const fn new(backing: B) -> Self {
        Self {
            backing,
            shared: Shared {
                lock: Lock::new(),
                regions: UnsafeCell::new([0; MAX_REGIONS]),
                region_count: UnsafeCell::new(0),
                bump_unit: UnsafeCell::new(UNITS_PER_REGION),
                free_spans: UnsafeCell::new([null_mut(); MAX_ORDER + 1]),
                free_committed: UnsafeCell::new([0; MAX_ORDER + 1]),
                orphans: UnsafeCell::new([null_mut(); CLASS_COUNT]),
                heap_pool: UnsafeCell::new(null_mut()),
                heap_chunk: UnsafeCell::new((0, 0)),
                table: [const { AtomicUsize::new(0) }; TABLE_SIZE],
            },
            fallback: Fallback {
                lock: Lock::new(),
                heap: UnsafeCell::new(LocalHeap::empty()),
            },
        }
    }

    /// 分配；失败返回空指针。
    ///
    /// # Safety
    /// 同 `GlobalAlloc::alloc`。
    pub unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let need = layout.size().max(layout.align()).max(1);
        if need > MAX_SMALL {
            return self
                .backing
                .map_large(layout.size(), layout.align())
                .map_or(null_mut(), |address| address as *mut u8);
        }
        // 对齐超过块自然对齐时取 2 的幂级别：2 的幂块在按块大小对齐的 span 内保持自然对齐。
        let class = if layout.align() > MIN_BLOCK {
            class_of(need.next_power_of_two())
        } else {
            class_of(need)
        };
        match self.backing.thread_slot() {
            Some(slot) => {
                let heap = self.thread_heap(slot);
                if heap.is_null() {
                    return null_mut();
                }
                // SAFETY: 线程槽里的堆只由本线程使用。
                unsafe { self.alloc_class(heap, class) }
            }
            None => {
                let _guard = self.fallback.lock.lock(&self.backing);
                // SAFETY: 后备堆在其锁内独占使用。
                unsafe { self.alloc_class(self.fallback.heap.get(), class) }
            }
        }
    }

    /// 释放。
    ///
    /// # Safety
    /// 同 `GlobalAlloc::dealloc`。
    pub unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let Some(span) = self.span_of(pointer as usize) else {
            self.backing.unmap_large(pointer as usize, layout.size());
            return;
        };
        let block = pointer.cast::<Block>();
        match self.backing.thread_slot() {
            Some(slot) => {
                let heap = slot.load(Ordering::Relaxed);
                // SAFETY: span 来自区域元数据；owner 比较决定本地或远端路径。
                unsafe {
                    if !heap.is_null() && (*span).owner.load(Ordering::Acquire) == heap {
                        self.free_local(heap, span, block);
                    } else {
                        self.free_remote(span, block);
                    }
                }
            }
            None => {
                let _guard = self.fallback.lock.lock(&self.backing);
                let heap = self.fallback.heap.get();
                // SAFETY: 同上；后备堆的本地路径在其锁内。
                unsafe {
                    if (*span).owner.load(Ordering::Acquire) == heap {
                        self.free_local(heap, span, block);
                    } else {
                        self.free_remote(span, block);
                    }
                }
            }
        }
    }

    /// 调整大小：块容量足够且级别不变时原地返回，否则分配、复制、释放。
    ///
    /// # Safety
    /// 同 `GlobalAlloc::realloc`。
    pub unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if let Some(span) = self.span_of(pointer as usize) {
            // SAFETY: span 元数据在块存活期间不被回收。
            let (capacity, class) =
                unsafe { ((*span).block_size as usize, usize::from((*span).class)) };
            let new_need = new_size.max(layout.align()).max(1);
            if new_need <= capacity && new_need <= MAX_SMALL && class_of(new_need) == class {
                return pointer;
            }
        }
        // SAFETY: 新布局沿用对齐，大小非零由调用者保证。
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        // SAFETY: 同 `alloc`。
        let new = unsafe { self.alloc(new_layout) };
        if !new.is_null() {
            // SAFETY: 两块不重叠且各自容纳 min 字节。
            unsafe {
                core::ptr::copy_nonoverlapping(pointer, new, layout.size().min(new_size));
                self.dealloc(pointer, layout);
            }
        }
        new
    }

    /// 线程退出：回收其堆。空 span 立即归还，其余转为孤儿供他人认领，堆结构回到池中。
    ///
    /// 必须在该线程不再分配/释放之后调用（通常由回收者在线程终止后调用）。
    pub fn retire_thread(&self, slot: &AtomicPtr<LocalHeap>) {
        let heap = slot.swap(null_mut(), Ordering::AcqRel);
        if heap.is_null() {
            return;
        }
        // SAFETY: 线程已终止，堆无其他访问者；span 元数据遵守所属协议。
        unsafe {
            self.drain_pending(heap);
            let mut span = (*heap).owned;
            while !span.is_null() {
                let next = (*span).owned_next;
                self.collect_remote(span);
                if (*span).used == 0 {
                    self.unlink_owned(heap, span);
                    self.unlink_partial(heap, span);
                    self.release_span(span);
                } else {
                    self.unlink_owned(heap, span);
                    self.unlink_partial(heap, span);
                    self.orphan(span);
                }
                span = next;
            }
            (*heap).active = [null_mut(); CLASS_COUNT];
            let _guard = self.shared.lock.lock(&self.backing);
            (*heap).next_pooled = *self.shared.heap_pool.get();
            *self.shared.heap_pool.get() = heap;
        }
    }

    // ---- 线程堆 ----

    fn thread_heap(&self, slot: &AtomicPtr<LocalHeap>) -> *mut LocalHeap {
        let heap = slot.load(Ordering::Relaxed);
        if !heap.is_null() {
            return heap;
        }
        let _guard = self.shared.lock.lock(&self.backing);
        // SAFETY: 全局锁内访问堆池。
        let heap = unsafe {
            let pooled = *self.shared.heap_pool.get();
            if pooled.is_null() {
                self.carve_heap()
            } else {
                *self.shared.heap_pool.get() = (*pooled).next_pooled;
                pooled
            }
        };
        if !heap.is_null() {
            slot.store(heap, Ordering::Relaxed);
        }
        heap
    }

    /// 从堆块里切出一个新的零初始化 `LocalHeap`；调用者持全局锁。
    unsafe fn carve_heap(&self) -> *mut LocalHeap {
        let size = size_of::<LocalHeap>().next_multiple_of(MIN_BLOCK);
        // SAFETY: 全局锁内访问。
        let chunk = unsafe { &mut *self.shared.heap_chunk.get() };
        if chunk.1 < size {
            let Some((base, _)) = self.backing.reserve(HEAP_CHUNK, UNIT) else {
                return null_mut();
            };
            *chunk = (base, HEAP_CHUNK);
        }
        let heap = chunk.0 as *mut LocalHeap;
        chunk.0 += size;
        chunk.1 -= size;
        // SAFETY: 刚切出的区间可写、按 16 字节对齐（指针字段需 8 字节）。
        unsafe { heap.write(LocalHeap::empty()) };
        heap
    }

    // ---- 分配 ----

    unsafe fn alloc_class(&self, heap: *mut LocalHeap, class: usize) -> *mut u8 {
        // SAFETY: `heap` 由调用线程独占；span 属于该堆。
        unsafe {
            let span = (*heap).active[class];
            if !span.is_null()
                && let Some(block) = take_block(span)
            {
                return block;
            }
            self.alloc_slow(heap, class)
        }
    }

    #[cold]
    unsafe fn alloc_slow(&self, heap: *mut LocalHeap, class: usize) -> *mut u8 {
        // SAFETY: 同 `alloc_class`。
        unsafe {
            self.drain_pending(heap);
            // 1. 当前 span 的远端释放可能已补充了空闲块。
            let active = (*heap).active[class];
            if !active.is_null() {
                self.collect_remote(active);
                if let Some(block) = take_block(active) {
                    return block;
                }
                // 已满的 span 不入任何链，释放回来时再挂 partial。
                (*heap).active[class] = null_mut();
            }
            // 2. 本堆的 partial，再到孤儿，最后新 span。
            loop {
                let span = (*heap).partial[class];
                if span.is_null() {
                    break;
                }
                self.unlink_partial(heap, span);
                (*heap).active[class] = span;
                if let Some(block) = take_block(span) {
                    return block;
                }
                (*heap).active[class] = null_mut();
            }
            while let Some(span) = self.adopt(heap, class) {
                (*heap).active[class] = span;
                if let Some(block) = take_block(span) {
                    return block;
                }
                (*heap).active[class] = null_mut();
            }
            let Some(span) = self.new_span(heap, class) else {
                return null_mut();
            };
            (*heap).active[class] = span;
            take_block(span).unwrap_or(null_mut())
        }
    }

    unsafe fn new_span(&self, heap: *mut LocalHeap, class: usize) -> Option<*mut SpanMeta> {
        let order = span_order(class);
        let span = {
            let _guard = self.shared.lock.lock(&self.backing);
            // SAFETY: 全局锁内。
            unsafe { self.acquire_span(order)? }
        };
        let size = block_size(class);
        let bytes = UNIT << order;
        // SAFETY: span 刚由全局栈或切分取得，当前独占。
        unsafe {
            (*span).free = null_mut();
            (*span).prev = null_mut();
            (*span).next = null_mut();
            (*span).block_size = size as u32;
            (*span).used = 0;
            (*span).bump = 0;
            (*span).limit = ((bytes / size) * size) as u32;
            (*span).class = class as u8;
            (*span).listed = false;
            (*span).committed = true;
            (*span).remote.store(null_mut(), Ordering::Relaxed);
            // `queued` 不重置：它可能仍标记着旧堆 pending 栈上的过期项，旧堆排空时会转交给现任主人。
            (*span).owner.store(heap, Ordering::Release);
            self.link_owned(heap, span);
        }
        Some(span)
    }

    // ---- 释放 ----

    unsafe fn free_local(&self, heap: *mut LocalHeap, span: *mut SpanMeta, block: *mut Block) {
        // SAFETY: 调用者保证 `span` 属于 `heap` 且 `block` 是其中一个已分配块。
        unsafe {
            (*block).next = (*span).free;
            (*span).free = block;
            (*span).used -= 1;
            self.after_free(heap, span);
        }
    }

    /// 本地空闲块增加之后维护 span 的链接：空 span 归还，无空位变有空位的挂 partial。
    unsafe fn after_free(&self, heap: *mut LocalHeap, span: *mut SpanMeta) {
        // SAFETY: 调用者保证 span 属于 heap。
        unsafe {
            let class = usize::from((*span).class);
            if (*heap).active[class] == span {
                return;
            }
            if (*span).used == 0 {
                self.unlink_partial(heap, span);
                self.unlink_owned(heap, span);
                self.release_span(span);
            } else if !(*span).listed {
                (*span).listed = true;
                (*span).prev = null_mut();
                (*span).next = (*heap).partial[class];
                if !(*span).next.is_null() {
                    (*(*span).next).prev = span;
                }
                (*heap).partial[class] = span;
            }
        }
    }

    unsafe fn free_remote(&self, span: *mut SpanMeta, block: *mut Block) {
        // SAFETY: span 在块存活期间不会被回收；块归还后由所属堆整体收走。
        unsafe {
            let mut head = (*span).remote.load(Ordering::Relaxed);
            loop {
                (*block).next = head;
                match (*span).remote.compare_exchange_weak(
                    head,
                    block,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(current) => head = current,
                }
            }
            let owner = (*span).owner.load(Ordering::Acquire);
            self.enqueue_pending(span, owner);
        }
    }

    /// 把 span 压入 `owner` 的 pending 栈；已在某个栈上或没有所属堆时什么都不做。
    unsafe fn enqueue_pending(&self, span: *mut SpanMeta, owner: *mut LocalHeap) {
        // SAFETY: LocalHeap 结构永不释放（回到池中复用），span 元数据永不回收。
        unsafe {
            if owner.is_null()
                || (*span)
                    .queued
                    .compare_exchange(null_mut(), owner, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
            {
                return;
            }
            let mut top = (*owner).pending.load(Ordering::Relaxed);
            loop {
                (*span).pending_next.store(top, Ordering::Relaxed);
                match (*owner).pending.compare_exchange_weak(
                    top,
                    span,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(current) => top = current,
                }
            }
        }
    }

    /// 把 span 的远端释放栈整体并入本地空闲链；调用者是所属堆（或独占者）。
    unsafe fn collect_remote(&self, span: *mut SpanMeta) {
        // SAFETY: 单消费者整体摘取，不存在 ABA。
        unsafe {
            let mut block = (*span).remote.swap(null_mut(), Ordering::Acquire);
            while !block.is_null() {
                let next = (*block).next;
                (*block).next = (*span).free;
                (*span).free = block;
                (*span).used -= 1;
                block = next;
            }
        }
    }

    unsafe fn drain_pending(&self, heap: *mut LocalHeap) {
        // SAFETY: `heap` 的 pending 由所属线程独占消费。
        unsafe {
            let mut span = (*heap).pending.swap(null_mut(), Ordering::Acquire);
            while !span.is_null() {
                let next = (*span).pending_next.load(Ordering::Relaxed);
                // 只清除属于本堆的标记，且先清再收集：之后到达的远端释放会重新入栈，不会丢失通知。
                // 标记已属于别的堆（span 在入栈后换了主人并被那里重新入栈）则这一项已过期，不能动它。
                if (*span)
                    .queued
                    .compare_exchange(heap, null_mut(), Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    let owner = (*span).owner.load(Ordering::Acquire);
                    if owner == heap {
                        self.collect_remote(span);
                        self.after_free(heap, span);
                    } else {
                        // span 在入栈后被孤儿化/认领：把待收集通知转交给现在的主人。
                        self.enqueue_pending(span, owner);
                    }
                }
                span = next;
            }
        }
    }

    // ---- 链表 ----

    unsafe fn link_owned(&self, heap: *mut LocalHeap, span: *mut SpanMeta) {
        // SAFETY: 调用者保证独占。
        unsafe {
            (*span).owned_prev = null_mut();
            (*span).owned_next = (*heap).owned;
            if !(*span).owned_next.is_null() {
                (*(*span).owned_next).owned_prev = span;
            }
            (*heap).owned = span;
        }
    }

    unsafe fn unlink_owned(&self, heap: *mut LocalHeap, span: *mut SpanMeta) {
        // SAFETY: span 在 heap 的 owned 链上。
        unsafe {
            let (prev, next) = ((*span).owned_prev, (*span).owned_next);
            if prev.is_null() {
                (*heap).owned = next;
            } else {
                (*prev).owned_next = next;
            }
            if !next.is_null() {
                (*next).owned_prev = prev;
            }
            (*span).owned_prev = null_mut();
            (*span).owned_next = null_mut();
        }
    }

    unsafe fn unlink_partial(&self, heap: *mut LocalHeap, span: *mut SpanMeta) {
        // SAFETY: `listed` 为真时 span 在对应级别的 partial 链上。
        unsafe {
            if !(*span).listed {
                return;
            }
            let (prev, next) = ((*span).prev, (*span).next);
            if prev.is_null() {
                (*heap).partial[usize::from((*span).class)] = next;
            } else {
                (*prev).next = next;
            }
            if !next.is_null() {
                (*next).prev = prev;
            }
            (*span).listed = false;
            (*span).prev = null_mut();
            (*span).next = null_mut();
        }
    }

    // ---- 全局 span 与孤儿 ----

    unsafe fn orphan(&self, span: *mut SpanMeta) {
        // SAFETY: span 已从原堆摘下；入孤儿链前先清 owner，之后远端释放只压 remote。
        unsafe {
            (*span).owner.store(null_mut(), Ordering::Release);
            let _guard = self.shared.lock.lock(&self.backing);
            let heads = &mut *self.shared.orphans.get();
            let class = usize::from((*span).class);
            (*span).prev = null_mut();
            (*span).next = heads[class];
            heads[class] = span;
        }
    }

    unsafe fn adopt(&self, heap: *mut LocalHeap, class: usize) -> Option<*mut SpanMeta> {
        // SAFETY: 全局锁内摘下孤儿后归本堆独占。
        unsafe {
            let span = {
                let _guard = self.shared.lock.lock(&self.backing);
                let heads = &mut *self.shared.orphans.get();
                let span = heads[class];
                if span.is_null() {
                    return None;
                }
                heads[class] = (*span).next;
                span
            };
            (*span).next = null_mut();
            (*span).owner.store(heap, Ordering::Release);
            self.link_owned(heap, span);
            self.collect_remote(span);
            if (*span).used == 0 {
                // 不能留空 span 当 active：释放它，让调用者继续找下一个。
                self.unlink_owned(heap, span);
                self.release_span(span);
                return self.adopt(heap, class);
            }
            Some(span)
        }
    }

    /// 取一个 `order` 阶的 span；调用者持全局锁。
    unsafe fn acquire_span(&self, order: usize) -> Option<*mut SpanMeta> {
        // SAFETY: 全局锁内访问全局状态。
        unsafe {
            let free = self.shared.free_spans.get();
            let span = (*free)[order];
            if !span.is_null() {
                (*free)[order] = (*span).next;
                (*span).next = null_mut();
                if (*span).committed {
                    (*self.shared.free_committed.get())[order] -= 1;
                }
                return Some(span);
            }
            let units = 1usize << order;
            let bump = self.shared.bump_unit.get();
            loop {
                if *bump + units > UNITS_PER_REGION {
                    self.new_region()?;
                    continue;
                }
                // 起点对齐到 span 大小：缝隙按“最大对齐块”拆成较小的空闲 span，不浪费。
                while !(*bump).is_multiple_of(units) {
                    let piece = (*bump).trailing_zeros() as usize;
                    let gap = self.region_span(*bump);
                    self.init_free_piece(gap, *bump, piece);
                    (*gap).next = (*self.shared.free_spans.get())[piece];
                    (*self.shared.free_spans.get())[piece] = gap;
                    *bump += 1 << piece;
                }
                let span = self.region_span(*bump);
                self.init_free_piece(span, *bump, order);
                *bump += units;
                return Some(span);
            }
        }
    }

    /// 当前区域第 `unit` 单元的 SpanMeta。
    unsafe fn region_span(&self, unit: usize) -> *mut SpanMeta {
        // SAFETY: 全局锁内，区域数组与计数一致。
        unsafe {
            let count = *self.shared.region_count.get();
            let base = (*self.shared.regions.get())[count - 1];
            let region = base as *mut RegionMeta;
            &raw mut (*region).spans[unit]
        }
    }

    unsafe fn init_free_piece(&self, span: *mut SpanMeta, unit: usize, order: usize) {
        // SAFETY: 全局锁内；元数据区全零起步，由本函数初始化为空闲 span。
        unsafe {
            let count = *self.shared.region_count.get();
            let base = (*self.shared.regions.get())[count - 1];
            let region = base as *mut RegionMeta;
            for covered in unit..unit + (1 << order) {
                (*region).start_of[covered] = unit as u16;
            }
            (*span).base = base + unit * UNIT;
            (*span).order = order as u8;
            (*span).committed = false;
            (*span).next = null_mut();
        }
    }

    /// 预留新区域并发布到查找表；调用者持全局锁。
    unsafe fn new_region(&self) -> Option<()> {
        // SAFETY: 全局锁内。
        unsafe {
            let count = *self.shared.region_count.get();
            if count == MAX_REGIONS {
                return None;
            }
            let (base, token) = self.backing.reserve(REGION_SIZE, REGION_SIZE)?;
            (*(base as *mut RegionMeta)).token = token;
            (*self.shared.regions.get())[count] = base;
            *self.shared.region_count.get() = count + 1;
            *self.shared.bump_unit.get() = META_UNITS;
            let key = base >> REGION_SHIFT;
            let mut slot = key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (usize::BITS as usize - 10);
            loop {
                let entry = &self.shared.table[slot % TABLE_SIZE];
                if entry.load(Ordering::Relaxed) == 0 {
                    entry.store(base, Ordering::Release);
                    break;
                }
                slot += 1;
            }
            Some(())
        }
    }

    /// 空 span 回到全局栈：超过保留数的先 decommit，使空闲服务不保留历史峰值。
    unsafe fn release_span(&self, span: *mut SpanMeta) {
        // SAFETY: span 已从所有堆链表摘下且没有已分配块。
        unsafe {
            (*span).owner.store(null_mut(), Ordering::Release);
            let order = usize::from((*span).order);
            let base = (*span).base;
            let region_base = base & !(REGION_SIZE - 1);
            let decommit = {
                let _guard = self.shared.lock.lock(&self.backing);
                (*self.shared.free_committed.get())[order] >= KEEP_COMMITTED
            };
            if decommit {
                let token = (*(region_base as *const RegionMeta)).token;
                self.backing
                    .decommit(token, base - region_base, UNIT << order);
            }
            let _guard = self.shared.lock.lock(&self.backing);
            (*span).committed = !decommit;
            if !decommit {
                (*self.shared.free_committed.get())[order] += 1;
            }
            let free = self.shared.free_spans.get();
            (*span).next = (*free)[order];
            (*free)[order] = span;
        }
    }

    /// 指针所属的 span；不在任何区域内（大对象）返回 `None`。
    fn span_of(&self, address: usize) -> Option<*mut SpanMeta> {
        let base = address & !(REGION_SIZE - 1);
        let mut slot = (base >> REGION_SHIFT).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            >> (usize::BITS as usize - 10);
        loop {
            match self.shared.table[slot % TABLE_SIZE].load(Ordering::Acquire) {
                0 => return None,
                entry if entry == base => break,
                _ => slot += 1,
            }
        }
        let region = base as *mut RegionMeta;
        let unit = (address - base) >> UNIT_SHIFT;
        // SAFETY: 区域元数据在进程生命周期内映射；数据区单元的 start_of 在 span 创建时写入。
        unsafe {
            let start = usize::from((*region).start_of[unit]);
            Some(&raw mut (*region).spans[start])
        }
    }
}

/// 从 span 取一块：先本地空闲链，再由 bump 游标增量切出。
unsafe fn take_block(span: *mut SpanMeta) -> Option<*mut u8> {
    // SAFETY: 调用者保证 span 属于调用线程的堆。
    unsafe {
        let block = (*span).free;
        if !block.is_null() {
            (*span).free = (*block).next;
            (*span).used += 1;
            return Some(block.cast());
        }
        let size = (*span).block_size;
        if (*span).bump + size <= (*span).limit {
            let address = (*span).base + (*span).bump as usize;
            (*span).bump += size;
            (*span).used += 1;
            return Some(address as *mut u8);
        }
        None
    }
}

const _: () = assert!(size_of::<RegionMeta>() <= META_UNITS * UNIT);

#[cfg(test)]
mod tests;
