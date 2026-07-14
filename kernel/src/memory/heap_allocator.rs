use ::alloc::vec::Vec;
use core::{
    alloc::{self, GlobalAlloc, Layout},
    cell::UnsafeCell,
    mem::size_of,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use spin::{Mutex, Once};

use super::{
    FrameAllocationClass,
    address::PhysicalPageNumber,
    config,
    frame_allocator::{self, FrameTracker},
};
use crate::sync::LocalIrqGuard;

struct HeapStorage(UnsafeCell<[u8; config::BOOTSTRAP_HEAP_SIZE]>);

// SAFETY: bootstrap storage 只由 BOOTSTRAP_OFFSET 在 IRQ-off lock 下分配；发布给调用方后
// 区间互不重叠，frame-backed allocator 启用后不再从该 storage 取得新对象。
unsafe impl Sync for HeapStorage {}

// OWNER: heap allocator 永久拥有 bootstrap bytes；早期对象可跨 allocator 切换存活。
static KERNEL_HEAP_MEMORY: HeapStorage =
    HeapStorage(UnsafeCell::new([0; config::BOOTSTRAP_HEAP_SIZE]));
static BOOTSTRAP_OFFSET: Mutex<usize> = Mutex::new(0);

const MIN_CLASS_SIZE: usize = core::mem::size_of::<usize>();
const CACHE_MAX_SIZE: usize = 256;
const SLAB_MAX_SIZE: usize = 1024;
const CACHE_CLASS_COUNT: usize =
    (CACHE_MAX_SIZE.trailing_zeros() - MIN_CLASS_SIZE.trailing_zeros() + 1) as usize;
const SLAB_CLASS_COUNT: usize =
    (SLAB_MAX_SIZE.trailing_zeros() - MIN_CLASS_SIZE.trailing_zeros() + 1) as usize;
const CACHE_REFILL_BLOCKS: usize = 8;
const CACHE_BLOCKS_PER_CLASS: u8 = 32;
const SLAB_MAGIC: usize = 0x4c53_4c41_4250_4147;
const DIRECT_MAGIC: usize = 0x4c53_4449_5245_4354;

#[repr(C)]
struct SlabHeader {
    magic: usize,
    class: usize,
    capacity: usize,
    free: usize,
    first_free: usize,
    previous: usize,
    next: usize,
}

#[repr(C)]
struct DirectHeader {
    magic: usize,
    pages: usize,
}

struct HeapState {
    // OWNER: 每个 head 唯一索引该 class 所有 non-full slab。SlabHeader 的双链只在
    // HEAP_STATE lock 下修改；缺失双链会让 full/empty transition 退化为全表扫描。
    slab_heads: [usize; SLAB_CLASS_COUNT],
    // OWNER: 与 slab list/header 同 transaction 更新的 live page projection。
    slab_pages: usize,
}

impl HeapState {
    const fn new() -> Self {
        Self {
            slab_heads: [0; SLAB_CLASS_COUNT],
            slab_pages: 0,
        }
    }

    fn insert_slab(&mut self, address: usize, class: usize) {
        let head = self.slab_heads[class];
        // SAFETY: address 是 heap lock 下刚初始化或从 full 转为 non-full 的 live slab page。
        let header = unsafe { &mut *(address as *mut SlabHeader) };
        debug_assert_eq!(header.magic, SLAB_MAGIC);
        debug_assert_eq!(header.previous, 0);
        debug_assert_eq!(header.next, 0);
        header.next = head;
        if head != 0 {
            // SAFETY: head 来自同 lock 保护的 class list。
            let old = unsafe { &mut *(head as *mut SlabHeader) };
            debug_assert_eq!(old.previous, 0);
            old.previous = address;
        }
        self.slab_heads[class] = address;
    }

    fn remove_slab(&mut self, address: usize, class: usize) {
        // SAFETY: caller 已证明 address 当前位于 class non-full list。
        let header = unsafe { &mut *(address as *mut SlabHeader) };
        if header.previous == 0 {
            assert_eq!(self.slab_heads[class], address, "slab list lost its head");
            self.slab_heads[class] = header.next;
        } else {
            // SAFETY: previous 是同一 live class list 中的 slab page。
            unsafe { (*(header.previous as *mut SlabHeader)).next = header.next };
        }
        if header.next != 0 {
            // SAFETY: next 是同一 live class list 中的 slab page。
            unsafe { (*(header.next as *mut SlabHeader)).previous = header.previous };
        }
        header.previous = 0;
        header.next = 0;
    }

    fn allocate_slab_block(&mut self, class: usize) -> Option<NonNull<u8>> {
        let address = self.slab_heads[class];
        if address == 0 {
            return None;
        }
        // SAFETY: list head 是 live non-full slab，所有 metadata 由当前 lock 独占。
        let header = unsafe { &mut *(address as *mut SlabHeader) };
        assert_eq!(header.magic, SLAB_MAGIC, "corrupt slab header");
        assert_eq!(header.class, class, "slab linked in wrong class");
        assert!(header.free != 0 && header.first_free != 0);
        let block = header.first_free;
        // SAFETY: free block 的首个 usize 在 free-list membership 期间唯一存放 next。
        header.first_free = unsafe { (block as *const usize).read() };
        header.free -= 1;
        if header.free == 0 {
            self.remove_slab(address, class);
        }
        NonNull::new(block as *mut u8)
    }

    fn publish_slab(&mut self, address: usize, class: usize) {
        // SAFETY: prepare_slab 完整初始化候选页，且 FrameTracker 在 publication 前保持独占。
        let header = unsafe { &*(address as *const SlabHeader) };
        assert_eq!(header.magic, SLAB_MAGIC, "unprepared slab page");
        assert_eq!(header.class, class, "prepared slab has wrong class");
        self.insert_slab(address, class);
        self.slab_pages = self
            .slab_pages
            .checked_add(1)
            .expect("slab page count overflow");
    }

    fn deallocate_slab_block(&mut self, address: usize, block: NonNull<u8>, class: usize) -> bool {
        // SAFETY: GlobalAlloc contract and page magic prove block belongs to this live slab.
        let header = unsafe { &mut *(address as *mut SlabHeader) };
        assert_eq!(header.magic, SLAB_MAGIC, "corrupt slab header");
        assert_eq!(header.class, class, "slab deallocated with wrong layout");
        assert!(header.free < header.capacity, "slab block double free");
        let was_full = header.free == 0;
        // SAFETY: caller returned exclusive block ownership; its first word may become free link.
        unsafe { block.as_ptr().cast::<usize>().write(header.first_free) };
        header.first_free = block.as_ptr() as usize;
        header.free += 1;
        if was_full {
            self.insert_slab(address, class);
        }
        if header.free == header.capacity {
            self.remove_slab(address, class);
            header.magic = 0;
            self.slab_pages = self
                .slab_pages
                .checked_sub(1)
                .expect("slab page count underflow");
            return true;
        }
        false
    }
}

// OWNER: global slab metadata has exactly one IRQ-safe mutation domain. Frame allocation and
// FrameTracker drop are deliberately outside this lock, preserving frame -> heap lock acyclicity.
static HEAP_STATE: Mutex<HeapState> = Mutex::new(HeapState::new());
// OWNER: direct extent 不进入 HEAP_STATE；该 counter 与 header publication/frame return
// 分别以 Relaxed 原子提交，只提供瞬时统计，不参与 lifetime 判定。
static DIRECT_PAGES: AtomicUsize = AtomicUsize::new(0);

/// @description global heap 当前占用的 frame-backed 物理页快照。
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapStatistics {
    /// live slab 与 direct extent 合计页数；不包含静态 bootstrap arena。
    pub(crate) resident_pages: usize,
}

struct HartHeapCache {
    heads: [usize; CACHE_CLASS_COUNT],
    counts: [u8; CACHE_CLASS_COUNT],
}

impl HartHeapCache {
    const fn new() -> Self {
        Self {
            heads: [0; CACHE_CLASS_COUNT],
            counts: [0; CACHE_CLASS_COUNT],
        }
    }

    fn pop(&mut self, class: usize) -> Option<NonNull<u8>> {
        let head = self.heads[class];
        if head == 0 {
            return None;
        }
        // SAFETY: head 只由当前 hart/class push 写入，指向同 canonical layout 的 block。
        self.heads[class] = unsafe { (head as *const usize).read() };
        self.counts[class] = self.counts[class]
            .checked_sub(1)
            .expect("per-hart heap cache count underflow");
        NonNull::new(head as *mut u8)
    }

    fn push(&mut self, class: usize, block: NonNull<u8>) {
        // SAFETY: caller 已交还 block 独占权；canonical class 容纳且对齐 next pointer。
        unsafe { block.as_ptr().cast::<usize>().write(self.heads[class]) };
        self.heads[class] = block.as_ptr() as usize;
        self.counts[class] = self.counts[class]
            .checked_add(1)
            .expect("per-hart heap cache count overflow");
    }
}

struct HartHeapCaches(Vec<UnsafeCell<HartHeapCache>>);

// SAFETY: vector 发布后不变；每个 cell 只由对应 compact hart 在本地 IRQ-off 期间访问。
unsafe impl Sync for HartHeapCaches {}

// OWNER: 唯一 per-hart small-block cache；cache 持有的 block 仍计入所属 slab allocated。
static HART_HEAP_CACHES: Once<HartHeapCaches> = Once::new();

// OWNER: 该 release/acquire flag 是 bootstrap -> frame-backed allocation 的唯一切换点。
static FRAME_BACKED_GROWTH: AtomicBool = AtomicBool::new(false);

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value & !(alignment - 1))
}

fn class_layout(layout: Layout, maximum: usize) -> Option<(usize, Layout)> {
    let size = layout
        .size()
        .checked_next_power_of_two()?
        .max(layout.align())
        .max(MIN_CLASS_SIZE);
    if size > maximum {
        return None;
    }
    let class = size.trailing_zeros() as usize - MIN_CLASS_SIZE.trailing_zeros() as usize;
    Some((class, Layout::from_size_align(size, size).ok()?))
}

fn class_size(class: usize) -> usize {
    MIN_CLASS_SIZE << class
}

fn prepare_slab(address: usize, class: usize) {
    let block_size = class_size(class);
    let first =
        align_up(address + size_of::<SlabHeader>(), block_size).expect("slab block start overflow");
    let capacity = (address + config::PAGE_SIZE - first) / block_size;
    assert!(capacity != 0, "slab class does not fit one page");
    for index in 0..capacity {
        let block = first + index * block_size;
        let next = if index + 1 < capacity {
            block + block_size
        } else {
            0
        };
        // SAFETY: candidate FrameTracker uniquely owns the page; blocks are disjoint and unpublished.
        unsafe { (block as *mut usize).write(next) };
    }
    // SAFETY: address is page-aligned and the header fits before first block.
    unsafe {
        (address as *mut SlabHeader).write(SlabHeader {
            magic: SLAB_MAGIC,
            class,
            capacity,
            free: capacity,
            first_free: first,
            previous: 0,
            next: 0,
        })
    };
}

fn cache_layout(layout: Layout) -> Option<(usize, Layout)> {
    class_layout(layout, CACHE_MAX_SIZE)
}

fn slab_layout(layout: Layout) -> Option<(usize, Layout)> {
    class_layout(layout, SLAB_MAX_SIZE)
}

fn current_hart_cache() -> Option<&'static UnsafeCell<HartHeapCache>> {
    HART_HEAP_CACHES
        .get()?
        .0
        .get(crate::arch::hart::current_hart_index())
}

fn bootstrap_bounds() -> (usize, usize) {
    let start = KERNEL_HEAP_MEMORY.0.get().cast::<u8>() as usize;
    (start, start + config::BOOTSTRAP_HEAP_SIZE)
}

fn bootstrap_allocate(layout: Layout) -> Option<NonNull<u8>> {
    let _irq = LocalIrqGuard::disable();
    let mut offset = BOOTSTRAP_OFFSET.lock();
    let (base, end) = bootstrap_bounds();
    let start = align_up(base.checked_add(*offset)?, layout.align())?;
    let allocation_end = start.checked_add(layout.size())?;
    if allocation_end > end {
        return None;
    }
    *offset = allocation_end - base;
    NonNull::new(start as *mut u8)
}

fn try_allocate_slab(layout: Layout) -> Option<NonNull<u8>> {
    let (slab_class, canonical) = slab_layout(layout)?;
    let _irq = LocalIrqGuard::disable();
    if let Some((cache_class, _)) = cache_layout(canonical)
        && let Some(cache) = current_hart_cache()
    {
        // SAFETY: local IRQ guard makes this hart's cell exclusively accessible.
        let cache = unsafe { &mut *cache.get() };
        if let Some(block) = cache.pop(cache_class) {
            return Some(block);
        }
        let mut heap = HEAP_STATE.lock();
        let first = heap.allocate_slab_block(slab_class)?;
        for _ in 1..CACHE_REFILL_BLOCKS {
            let Some(block) = heap.allocate_slab_block(slab_class) else {
                break;
            };
            cache.push(cache_class, block);
        }
        return Some(first);
    }
    HEAP_STATE.lock().allocate_slab_block(slab_class)
}

fn publish_slab_and_allocate(layout: Layout, frames: FrameTracker) -> Option<NonNull<u8>> {
    let (class, _) = slab_layout(layout)?;
    let address = frames.ppn.as_usize().checked_mul(config::PAGE_SIZE)?;
    // 1. free-chain construction scales with blocks/page, so keep it outside IRQ-off heap lock.
    prepare_slab(address, class);
    let allocation = {
        let _irq = LocalIrqGuard::disable();
        let mut heap = HEAP_STATE.lock();
        // 2. publication and first allocation are one constant-time owner transaction.
        heap.publish_slab(address, class);
        heap.allocate_slab_block(class)
            .expect("new slab must satisfy its class")
    };
    // SlabHeader now owns the frame until its final backend block is returned.
    core::mem::forget(frames);
    Some(allocation)
}

fn direct_pages(layout: Layout) -> Option<usize> {
    let bytes = size_of::<DirectHeader>()
        .checked_add(layout.align() - 1)?
        .checked_add(layout.size())?;
    bytes
        .max(layout.align())
        .div_ceil(config::PAGE_SIZE)
        .checked_next_power_of_two()
}

fn allocate_direct(layout: Layout) -> Option<NonNull<u8>> {
    let requested_pages = direct_pages(layout)?;
    let frames =
        frame_allocator::alloc_contiguous(requested_pages, FrameAllocationClass::KernelHeap)?;
    let address = frames.ppn.as_usize().checked_mul(config::PAGE_SIZE)?;
    let allocation = align_up(address + size_of::<DirectHeader>(), layout.align())?;
    let extent_bytes = frames.pages.checked_mul(config::PAGE_SIZE)?;
    assert!(
        allocation
            .checked_add(layout.size())
            .is_some_and(|end| end <= address + extent_bytes),
        "direct heap layout exceeds frame extent"
    );
    // SAFETY: FrameTracker uniquely owns the extent and header lies before the returned payload.
    unsafe {
        (address as *mut DirectHeader).write(DirectHeader {
            magic: DIRECT_MAGIC,
            pages: frames.pages,
        })
    };
    DIRECT_PAGES.fetch_add(frames.pages, Ordering::Relaxed);
    core::mem::forget(frames);
    NonNull::new(allocation as *mut u8)
}

fn grow_and_allocate(layout: Layout) -> Option<NonNull<u8>> {
    if !FRAME_BACKED_GROWTH.load(Ordering::Acquire) {
        return bootstrap_allocate(layout);
    }
    if slab_layout(layout).is_some() {
        // Frame allocation/direct reclaim 必须在 IRQ-on 且不持 heap lock 时执行。
        let frames = frame_allocator::alloc_contiguous(1, FrameAllocationClass::KernelHeap);
        let Some(frames) = frames else {
            // 另一 hart 可能在 reclaim 窗口发布了同 class slab。
            return try_allocate_slab(layout);
        };
        return publish_slab_and_allocate(layout, frames);
    }
    allocate_direct(layout)
}

fn deallocate_backend(ptr: NonNull<u8>, layout: Layout) {
    let address = ptr.as_ptr() as usize;
    if let Some((class, _)) = slab_layout(layout) {
        let page = address & !(config::PAGE_SIZE - 1);
        let empty = {
            let _irq = LocalIrqGuard::disable();
            HEAP_STATE.lock().deallocate_slab_block(page, ptr, class)
        };
        if empty {
            // SAFETY: empty transition removed the page from every slab list and no block remains
            // published; this recreates the unique frame owner outside the heap lock.
            drop(unsafe {
                FrameTracker::from_raw(PhysicalPageNumber::from(page / config::PAGE_SIZE), 1)
            });
        }
        return;
    }

    let pages = direct_pages(layout).expect("direct heap layout no longer representable");
    let extent_bytes = pages
        .checked_mul(config::PAGE_SIZE)
        .expect("direct heap extent overflow");
    let base = address & !(extent_bytes - 1);
    // SAFETY: layout reproduces the order-aligned extent base chosen by allocate_direct.
    let header = unsafe { &mut *(base as *mut DirectHeader) };
    assert_eq!(header.magic, DIRECT_MAGIC, "heap pointer has no live owner");
    assert_eq!(header.pages, pages, "direct heap layout mismatch");
    header.magic = 0;
    // SAFETY: header proved this allocation uniquely owns the complete extent.
    drop(unsafe {
        FrameTracker::from_raw(PhysicalPageNumber::from(base / config::PAGE_SIZE), pages)
    });
    DIRECT_PAGES
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_sub(pages)
        })
        .expect("direct heap page count underflow");
}

pub(crate) struct KernelAllocator;

// SAFETY: bootstrap bump ranges never overlap; frame-backed slab/direct ownership is serialized by
// HEAP_STATE or represented by one direct header, and same-hart cache mutation is IRQ-safe.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if layout.size() == 0 {
            return layout.align() as *mut u8;
        }
        if !FRAME_BACKED_GROWTH.load(Ordering::Acquire) {
            return bootstrap_allocate(layout).map_or(core::ptr::null_mut(), NonNull::as_ptr);
        }
        try_allocate_slab(layout)
            .or_else(|| grow_and_allocate(layout))
            .map_or(core::ptr::null_mut(), NonNull::as_ptr)
    }

    // SAFETY: caller must return the exact pointer/layout pair produced by this allocator.
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() == 0 || ptr.is_null() {
            return;
        }
        let address = ptr as usize;
        let (bootstrap_start, bootstrap_end) = bootstrap_bounds();
        if (bootstrap_start..bootstrap_end).contains(&address) {
            // Bootstrap allocations are bounded boot-lifetime state; the arena is never transferred
            // to frame allocator, so ignoring later drops cannot create overlapping ownership.
            return;
        }
        let block = NonNull::new(ptr).expect("non-null heap pointer");
        let _irq = LocalIrqGuard::disable();
        if let Some((class, _)) = cache_layout(layout)
            && let Some(cache) = current_hart_cache()
        {
            // SAFETY: local IRQ guard makes this hart's cell exclusively accessible.
            let cache = unsafe { &mut *cache.get() };
            if cache.counts[class] < CACHE_BLOCKS_PER_CLASS {
                cache.push(class, block);
                return;
            }
        }
        drop(_irq);
        deallocate_backend(block, layout);
    }
}

#[global_allocator]
// OWNER: Rust allocation ABI delegates exclusively to this adapter.
static HEAP_ALLOCATOR: KernelAllocator = KernelAllocator;

#[alloc_error_handler]
pub(crate) fn handle_heap_alloc_error(layout: alloc::Layout) -> ! {
    panic!("allocate heap memory error, layout = {:?}", layout);
}

pub(crate) fn init() {
    *BOOTSTRAP_OFFSET.lock() = 0;
}

/// @description 按已发布的动态 hart topology 构造 per-hart 小对象 cache。
/// @return 无返回值；每个 topology hart 恰有一个 cache cell。
/// @errors topology 未发布、零 hart 或重复初始化时 fail-stop。
pub(crate) fn init_hart_caches() {
    assert!(
        crate::arch::hart::topology_ready(),
        "heap caches require initialized hart topology"
    );
    assert!(
        HART_HEAP_CACHES.get().is_none(),
        "heap caches initialized twice"
    );
    let hart_count = crate::arch::hart::hart_count();
    assert!(hart_count != 0, "heap caches require at least one hart");
    let mut caches = Vec::new();
    caches
        .try_reserve_exact(hart_count)
        .expect("hart heap-cache allocation failed");
    for _ in 0..hart_count {
        caches.push(UnsafeCell::new(HartHeapCache::new()));
    }
    HART_HEAP_CACHES.call_once(|| HartHeapCaches(caches));
}

/// @description 在 frame allocator 初始化后原子切换到可回收的 slab/direct heap。
/// @return 无返回值；重复调用保持启用状态。
pub(crate) fn enable_frame_backed_growth() {
    FRAME_BACKED_GROWTH.store(true, Ordering::Release);
}

/// @description 读取唯一 heap owners 的常数时间 resident page projection。
/// @return slab 与 direct extent 合计页数，不扫描 slab、cache 或 allocation。
pub(crate) fn statistics() -> HeapStatistics {
    let _irq = LocalIrqGuard::disable();
    let slab_pages = HEAP_STATE.lock().slab_pages;
    HeapStatistics {
        resident_pages: slab_pages.saturating_add(DIRECT_PAGES.load(Ordering::Relaxed)),
    }
}
