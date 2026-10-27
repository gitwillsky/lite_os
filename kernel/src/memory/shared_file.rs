use crate::{fallible_tree::FallibleMap, sync::TaskMutexWaitPreparation};
use alloc::sync::{Arc, Weak};
use core::{
    fmt::Debug,
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
};
use spin::{Mutex, Once};

use super::{
    address::PhysicalPageNumber,
    config::PAGE_SIZE,
    frame_allocator,
    swappable_page::{PagePin, SwappablePage},
    vmstat::{self, PageState},
};

/// 磁盘页缓存页的 Linux mapcount：首次被映射时计入 `Mapped`，最后一个映射解除时扣除。
///
/// 只由 MM 的文件页映射记录（`FilePageMapping`）增减；缺失时同一页被多个进程映射会在
/// `Mapped` 中重复计数，或未映射页残留在统计里。
#[derive(Debug, Default)]
pub(crate) struct PageMapCount(AtomicUsize);

impl PageMapCount {
    pub(in crate::memory) fn map(&self) {
        if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
            vmstat::add(PageState::FileMapped, 1);
        }
    }

    pub(in crate::memory) fn unmap(&self) {
        let previous = self.0.fetch_sub(1, Ordering::Relaxed);
        debug_assert_ne!(previous, 0, "file page unmapped twice");
        if previous == 1 {
            vmstat::sub(PageState::FileMapped, 1);
        }
    }
}

/// 跨 fs/memory seam 标识一个 mounted filesystem inode。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SharedFileId {
    pub(crate) filesystem: usize,
    pub(crate) inode: u64,
}

/// page-cache 获取共享页时的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedFileError {
    OutOfMemory,
    Io,
    BeyondEof,
}

/// 可同时被用户页表与 kernel page cache 引用的单页物理存储。
#[derive(Debug)]
pub(crate) struct SharedFrame {
    frame: frame_allocator::FrameTracker,
}

impl SharedFrame {
    /// Transfer one unpublished 4KiB physical frame without copying its initialized bytes.
    pub(in crate::memory) fn from_frame(mut frame: frame_allocator::FrameTracker) -> Self {
        assert_eq!(
            frame.bytes_mut().len(),
            PAGE_SIZE,
            "shared content requires one frame"
        );
        Self { frame }
    }
    /// 分配并清零一个共享物理页。
    ///
    /// # Returns
    ///
    /// 成功返回唯一 owner；物理内存耗尽返回 OutOfMemory。
    pub(crate) fn allocate() -> Result<Self, SharedFileError> {
        frame_allocator::alloc()
            .map(|frame| Self { frame })
            .ok_or(SharedFileError::OutOfMemory)
    }

    /// 返回页表映射使用的物理页号。
    pub(crate) fn ppn(&self) -> PhysicalPageNumber {
        self.frame.ppn
    }

    /// Full COW copy skips redundant zero-fill; caller's PageOp pins the source through allocation.
    pub(in crate::memory) fn copy_frame(&self) -> Option<frame_allocator::FrameTracker> {
        frame_allocator::alloc_copy(self.frame.bytes())
    }

    /// 独占借用尚未发布的共享页内容，供 page-cache miss 直接填充 storage bytes。
    ///
    /// # Returns
    ///
    /// 生命周期绑定到 `SharedFrame` 独占借用的完整页切片。
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        self.frame.bytes_mut()
    }

    /// 从共享页复制到 kernel buffer。
    pub(crate) fn read(&self, offset: usize, output: &mut [u8]) {
        assert!(offset <= PAGE_SIZE && output.len() <= PAGE_SIZE - offset);
        // SAFETY: frame 在 self 生命周期内保持分配；范围已验证。U-mode 可并发修改该页，
        // 与 Linux shared mapping 相同，未同步的并发访问允许观察逐字节的中间结果。
        unsafe {
            ptr::copy_nonoverlapping(
                self.frame.ppn.as_page_ptr().add(offset),
                output.as_mut_ptr(),
                output.len(),
            )
        };
    }

    /// 从 kernel buffer 写入共享页并立即对现有 PTE 可见。
    pub(crate) fn write(&self, offset: usize, input: &[u8]) {
        assert!(offset <= PAGE_SIZE && input.len() <= PAGE_SIZE - offset);
        // SAFETY: frame 在 self 生命周期内保持分配；范围已验证。raw physical write 不创建
        // 与硬件 page-table walker 并存的 Rust 引用，用户并发访问遵循 shared-memory 语义。
        unsafe {
            ptr::copy_nonoverlapping(
                input.as_ptr(),
                self.frame.ppn.as_page_mut_ptr().add(offset),
                input.len(),
            )
        };
    }

    /// 将页内指定偏移到页尾清零，供 truncate 隐藏旧 EOF 尾部数据。
    pub(crate) fn zero_from(&self, offset: usize) {
        assert!(offset <= PAGE_SIZE);
        // SAFETY: offset 已限制在当前 live frame 内，write_bytes 不越过页尾。
        unsafe {
            ptr::write_bytes(
                self.frame.ppn.as_page_mut_ptr().add(offset),
                0,
                PAGE_SIZE - offset,
            )
        };
    }
}

/// MemorySet 持有的共享 cache page interface。
pub(crate) trait SharedPage: Send + Sync + Debug {
    /// Pin the current physical bytes through kernel access or MM leaf publication.
    fn pin(&self) -> Result<SharedPagePin<'_>, SharedFileError>;
    /// Disk-cache pages stay pinned; memory-backed pages share one canonical swap owner.
    fn swappable(&self) -> Option<&Arc<SwappablePage>> {
        None
    }
    fn acquire_writer(&self);
    fn release_writer(&self);
    /// 磁盘页缓存页的 mapcount；内存文件页的映射由 SwappablePage 引用计数，返回 None。
    fn map_count(&self) -> Option<&PageMapCount> {
        None
    }
}

/// Shared page physical access lifetime; disk cache borrows its fixed frame, tmpfs holds PageOp.
pub(crate) enum SharedPagePin<'a> {
    Cached(&'a SharedFrame),
    Swappable(PagePin<'a>),
}

impl SharedPagePin<'_> {
    pub(crate) fn ppn(&self) -> PhysicalPageNumber {
        match self {
            Self::Cached(frame) => frame.ppn(),
            Self::Swappable(pin) => pin.ppn(),
        }
    }
    pub(crate) fn read(&self, offset: usize, output: &mut [u8]) {
        match self {
            Self::Cached(frame) => frame.read(offset, output),
            Self::Swappable(pin) => pin.read(offset, output),
        }
    }
}

/// mmap 必须保活实际 file object 的全部资源（写者、bdev claim、backend 与 close lifecycle）。
/// 此窄 lifetime seam 不执行回调，不令 memory 反向依赖 file 或重新按 opened inode 获取 lease。
pub(crate) trait MappedFileOwner: core::any::Any + Send + Sync + Debug {}

/// file-backed shared VMA 消费的 page-cache interface。
pub(crate) trait SharedFileMapping: Send + Sync + Debug {
    fn id(&self) -> SharedFileId;
    fn size(&self) -> u64;
    /// Live backing write admission; freezing swap storage removes shared leaf W, not VMA W.
    fn writable(&self) -> bool {
        true
    }
    /// Original fd-derived VM_MAYWRITE, retained across close/fork/protection changes.
    fn may_write(&self) -> bool {
        true
    }
    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError>;
    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError>;
}

/// memory subsystem 对 live AddressSpace 的反向维护 interface。
pub(crate) trait MemoryMappingOwner: Send + Sync {
    /// Try-lock once, revoke/fence all aliases, count actual references including PROT_NONE.
    /// None cancels migration; this callback must never wait for MM or acquire PageOp.
    fn revoke_swappable_page(&self, page: &SwappablePage, ppn: PhysicalPageNumber)
    -> Option<usize>;
    /// Revoke shared writable translations and retire their writer claims after the remote fence.
    fn revoke_shared_file_writers(&self, id: SharedFileId, wait: &mut TaskMutexWaitPreparation);
    /// 撤销指定文件新 EOF 外的全部 live translation。
    ///
    /// # Parameters
    ///
    /// - `id`: mounted inode identity。
    /// - `size`: 已提交的文件长度。
    /// - `wait`: truncate 在 storage mutation 前预分配、可跨 owner 复用的 waiter。
    ///
    /// # Returns
    ///
    /// 无返回值；preparation 保证本提交尾部不再分配或失败。
    fn invalidate_shared_file(
        &self,
        id: SharedFileId,
        size: u64,
        wait: &mut TaskMutexWaitPreparation,
    );

    /// 在 swapoff 已禁止新增换出后，恢复指定 storage 的全部页；wait 在 Draining 前准备。
    fn restore_swapped_pages(
        &self,
        identity: SharedFileId,
        wait: &mut TaskMutexWaitPreparation,
    ) -> Result<(), SharedFileError>;
}

/// 一次 direct-reclaim adapter 调用的页目标与扫描上限。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReclaimRequest {
    target_pages: usize,
    scan_pages: usize,
    // CAPABILITY: 只有后台执行体可阻塞 swap I/O；allocator/direct reclaim 不可递归等待设备。
    allow_swap_io: bool,
}

impl ReclaimRequest {
    /// 为物理页目标建立固定放大率的扫描预算。
    ///
    /// # Parameters
    ///
    /// - `target_pages`: 本轮最多需要释放的物理页数。
    ///
    /// # Returns
    ///
    /// 扫描预算至少覆盖 256 个 resident entry，且不会发生整数溢出。
    pub(crate) fn for_target(target_pages: usize) -> Self {
        Self {
            target_pages,
            scan_pages: target_pages.saturating_mul(16).max(256),
            allow_swap_io: false,
        }
    }

    fn with_scan_budget(target_pages: usize, scan_pages: usize) -> Self {
        Self {
            target_pages,
            scan_pages,
            allow_swap_io: false,
        }
    }

    /// 返回本 adapter 最多需要释放的物理页数。
    pub(crate) fn target_pages(self) -> usize {
        self.target_pages
    }

    /// 返回本 adapter 最多可检查的 resident entry 数。
    pub(crate) fn scan_pages(self) -> usize {
        self.scan_pages
    }

    pub(crate) fn allows_swap_io(self) -> bool {
        self.allow_swap_io
    }
    pub(in crate::memory) fn limit_scans(&mut self, limit: usize) {
        self.scan_pages = self.scan_pages.min(limit);
    }
}

/// 一次 direct-reclaim adapter 调用的实际工作量。
#[must_use = "reclaim progress and scan cost must be propagated to the caller"]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReclaimResult {
    reclaimed_pages: usize,
    scanned_pages: usize,
}

/// direct reclaim 唯一 registry owner 的累计工作量。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReclaimStatistics {
    /// 非零页目标的 direct-reclaim 调用数。
    pub(crate) attempts: u64,
    /// 所有 adapter 实际检查的 resident entry 数。
    pub(crate) scanned_pages: u64,
    /// 实际归还 frame allocator 的物理页数。
    pub(crate) reclaimed_pages: u64,
}

/// 唯一 registry 下分别提交的直接与后台回收计数；不重复 resident owner。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReclaimStatisticsSnapshot {
    pub(crate) direct: ReclaimStatistics,
    pub(crate) background: ReclaimStatistics,
}

#[derive(Clone, Copy)]
enum ReclaimSource {
    Direct,
    Background,
}

impl ReclaimResult {
    /// 返回已归还 frame allocator 的页数；撤销一个共享映射不计作释放。
    pub(in crate::memory) const fn reclaimed_pages(self) -> usize {
        self.reclaimed_pages
    }

    pub(in crate::memory) const fn scanned_pages(self) -> usize {
        self.scanned_pages
    }

    /// 构造 adapter 的实际回收结果。
    ///
    /// # Parameters
    ///
    /// - `reclaimed_pages`: 已释放回 frame allocator 的物理页数。
    /// - `scanned_pages`: 已检查的 resident entry 数。
    ///
    /// # Returns
    ///
    /// 保留两项独立计数的结果；撤销共享 frame 的单个映射不会伪报物理页释放。
    pub(crate) const fn new(reclaimed_pages: usize, scanned_pages: usize) -> Self {
        Self {
            reclaimed_pages,
            scanned_pages,
        }
    }
}

/// frame allocator 慢路径使用的物理页回收 seam；具体 owner 不泄漏到 memory 下层。
pub(crate) trait MemoryReclaimer: Send + Sync {
    /// 在 adapter 自己的 owner lock 下执行一次有界 direct reclaim。
    ///
    /// # Parameters
    ///
    /// - `request`: 需要释放的页目标与允许检查的 resident entry 上限。
    ///
    /// # Returns
    ///
    /// 实际释放和扫描的页数；两项都不得超过 request 对应上限。
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult;
}

// OWNER: memory module owns stable weak address-space invalidation slots. Holding strong Process
// references here would keep exited address spaces and every mapped page alive forever; removing
// dead slots would shift a concurrent truncate scan and leave a live AddressSpace uninformed.
struct MappingRegistry {
    slots: FallibleMap<usize, Weak<dyn MemoryMappingOwner>>,
    // OWNER: 同锁记录 append 与 tombstone reuse；swapoff 必须重扫发生过 publication 的轮次，
    // 否则晚登记 fork 可在已扫描 slot 重新继承待 drain 的 swap reference。
    generation: u64,
}
static MEMORY_MAPPING_OWNERS: Once<Mutex<MappingRegistry>> = Once::new();

fn mapping_registry() -> &'static Mutex<MappingRegistry> {
    MEMORY_MAPPING_OWNERS.call_once(|| {
        Mutex::new(MappingRegistry {
            slots: FallibleMap::new(),
            generation: 0,
        })
    })
}
struct ReclaimerRegistry {
    // OWNER: 连续 key 是稳定 slot identity；只追加或复用 tombstone，从不删除节点。
    // 锁外准备节点，防止 heap growth 的 direct reclaim 重新取得正在持有的 registry lock。
    slots: FallibleMap<usize, Weak<dyn MemoryReclaimer>>,
    // OWNER: cursor 与 stable weak slots 由同一 registry lock 拥有，只表示下一个
    // direct-reclaim adapter。缺失它会让达到 quota 的短扫描每次都从首 slot
    // 开始，长期饿死后续 AddressSpace/page-cache owner。
    cursor: usize,
    // OWNER: statistics 是同一 registry lock 下对已完成 reclaim transaction 的
    // 只读 projection。缺失单一提交点会让 pgscan/pgsteal 跨 owner 重复计数。
    statistics: ReclaimStatisticsSnapshot,
}

// OWNER: memory module 只保存 weak reclaimer adapter 与单一轮转游标；强引用会让
// 已退出 AddressSpace 或已移除 page-cache object 永久存活。callback 前释放
// registry lock，避免 owner 锁序反转。
static MEMORY_RECLAIMERS: Once<Mutex<ReclaimerRegistry>> = Once::new();

fn reclaimer_registry() -> &'static Mutex<ReclaimerRegistry> {
    MEMORY_RECLAIMERS.call_once(|| {
        Mutex::new(ReclaimerRegistry {
            slots: FallibleMap::new(),
            cursor: 0,
            statistics: ReclaimStatisticsSnapshot::default(),
        })
    })
}

fn reuse_dead_owner_slot<T: ?Sized>(
    slots: &mut FallibleMap<usize, Weak<T>>,
    owner: &Arc<T>,
) -> bool {
    let index = slots
        .iter()
        .find(|(_, slot)| slot.strong_count() == 0)
        .map(|(&index, _)| index);
    let Some(index) = index else {
        return false;
    };
    *slots
        .get_mut(&index)
        .expect("stable owner slot disappeared") = Arc::downgrade(owner);
    true
}

fn contains_owner<T: ?Sized>(slots: &FallibleMap<usize, Weak<T>>, owner: &Arc<T>) -> bool {
    let identity = Arc::downgrade(owner);
    slots.iter().any(|(_, slot)| Weak::ptr_eq(slot, &identity))
}

/// 注册一个 address-space invalidator，registry 只保留 weak lifetime。
pub(crate) fn register_memory_mapping_owner(
    owner: Arc<dyn MemoryMappingOwner>,
) -> Result<(), SharedFileError> {
    let registry = mapping_registry();
    {
        let mut owners = registry.lock();
        if contains_owner(&owners.slots, &owner) {
            return Ok(());
        }
        if reuse_dead_owner_slot(&mut owners.slots, &owner) {
            owners.generation = owners
                .generation
                .checked_add(1)
                .expect("mapping publication generation exhausted");
            return Ok(());
        }
    }
    let slot = FallibleMap::<usize, Weak<dyn MemoryMappingOwner>>::try_reserve_node()
        .map_err(|_| SharedFileError::OutOfMemory)?;
    let mut owners = registry.lock();
    if contains_owner(&owners.slots, &owner) {
        drop(owners);
        return Ok(());
    }
    // preparation 期间其他注册或退出可产生 tombstone；复查后只提交仍需要的新 slot。
    if !reuse_dead_owner_slot(&mut owners.slots, &owner) {
        let index = owners.slots.len();
        owners
            .slots
            .commit_vacant(slot.fill(index, Arc::downgrade(&owner)));
    }
    owners.generation = owners
        .generation
        .checked_add(1)
        .expect("mapping publication generation exhausted");
    drop(owners);
    // 未使用的 NodeSlot 在 owner lock 外析构，回收 backing 不参与注册 transaction。
    Ok(())
}

/// Registered empty mm 在持有构造 guards 时完成 page ownership 初始化；不分配、不回调。
/// swapoff 先扫空 child 后等待 parent 时，必须看到这一继承提交并重扫已有 low slot。
pub(crate) fn complete_memory_mapping_owner_initialization() {
    let mut owners = mapping_registry().lock();
    owners.generation = owners
        .generation
        .checked_add(1)
        .expect("mapping publication generation exhausted");
}

/// 注册一个只保留 weak lifetime 的物理页回收 owner。
pub(crate) fn register_memory_reclaimer(
    owner: Arc<dyn MemoryReclaimer>,
) -> Result<(), SharedFileError> {
    {
        let mut registry = reclaimer_registry().lock();
        if reuse_dead_owner_slot(&mut registry.slots, &owner) {
            return Ok(());
        }
    }
    let slot = FallibleMap::<usize, Weak<dyn MemoryReclaimer>>::try_reserve_node()
        .map_err(|_| SharedFileError::OutOfMemory)?;
    let mut registry = reclaimer_registry().lock();
    if !reuse_dead_owner_slot(&mut registry.slots, &owner) {
        let index = registry.slots.len();
        registry
            .slots
            .commit_vacant(slot.fill(index, Arc::downgrade(&owner)));
    }
    drop(registry);
    Ok(())
}

/// 在不分配内存且不持有 registry lock 回调的前提下撤销所有 EOF 外 PTE。
///
/// # Parameters
///
/// - `id`: 已完成 storage truncate 的 mounted inode identity。
/// - `size`: 已提交的新文件字节长度。
/// - `wait`: storage mutation 前已成功准备的 blocking-acquisition metadata。
///
/// # Returns
///
/// 所有本轮可见的 live AddressSpace 均已完成 invalidation。
pub(crate) fn invalidate_shared_file(
    id: SharedFileId,
    size: u64,
    wait: &mut TaskMutexWaitPreparation,
) {
    let registry = mapping_registry();
    // 1. 固定本轮起点时已发布的 slot 数。随后登记或复用已扫 slot 的 AddressSpace
    // 在首次继承 shared/private resident 前检查已公布 EOF，不能复制旧 EOF 整页；
    // 首次 fault 也服从 backing EOF，因此未纳入本轮的 owner 不依赖再次扫描。
    let slot_count = registry.lock().slots.len();
    for index in 0..slot_count {
        // 2. 每次只在 registry lock 内 clone 一个 Weak。若建立 live Vec，truncate 已提交后
        // 仍可能因 snapshot OOM 返回，并永久留下 EOF 外 stale PTE。
        let owner = registry.lock().slots.get(&index).cloned();
        // 3. callback 前释放 registry lock，避免长时间 TLB shootdown 阻塞新 mm publication；
        // owner 在 clone 后退出是安全的，已经销毁的页表不再需要 invalidation。
        if let Some(owner) = owner.and_then(|owner| owner.upgrade()) {
            owner.invalidate_shared_file(id, size, wait);
        }
    }
}

/// 复用唯一 mapping registry 完成 swapoff；registry lock 不跨 callback/IO，publication 后重扫。
pub(in crate::memory) fn restore_swapped_pages<E: From<SharedFileError>>(
    identity: SharedFileId,
    wait: &mut TaskMutexWaitPreparation,
    check: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    maintain_mapping_owners(check, |owner| {
        owner
            .restore_swapped_pages(identity, wait)
            .map_err(Into::into)
    })
}

/// Freeze maintenance shares the same generation-aware live-mm walk as swapoff.
pub(crate) fn revoke_shared_file_writers(id: SharedFileId, wait: &mut TaskMutexWaitPreparation) {
    maintain_mapping_owners(
        || Ok(()),
        |owner| {
            owner.revoke_shared_file_writers(id, wait);
            Ok::<(), SharedFileError>(())
        },
    )
    .expect("writer retirement is infallible after preparation");
}

fn maintain_mapping_owners<E>(
    mut check: impl FnMut() -> Result<(), E>,
    mut maintain: impl FnMut(&dyn MemoryMappingOwner) -> Result<(), E>,
) -> Result<(), E> {
    let registry = mapping_registry();
    loop {
        let (count, generation) = {
            let owners = registry.lock();
            (owners.slots.len(), owners.generation)
        };
        for index in 0..count {
            check()?;
            let owner = registry.lock().slots.get(&index).cloned();
            if let Some(owner) = owner.and_then(|owner| owner.upgrade()) {
                maintain(owner.as_ref())?;
            }
        }
        if registry.lock().generation == generation {
            return Ok(());
        }
    }
}

/// One stable-slot walk, never a generation retry: each MM contributes to alias coverage once.
/// Registration is idempotent per Arc; counted references retain dead-MM retirement evidence.
pub(in crate::memory) fn revoke_swappable_page(
    page: &SwappablePage,
    ppn: PhysicalPageNumber,
) -> Option<usize> {
    let registry = mapping_registry();
    let count = registry.lock().slots.len();
    let mut fenced = 0usize;
    for index in 0..count {
        let owner = registry.lock().slots.get(&index).cloned();
        if let Some(owner) = owner.and_then(|owner| owner.upgrade()) {
            fenced = fenced.checked_add(owner.revoke_swappable_page(page, ppn)?)?;
        }
    }
    Some(fenced)
}

/// 轮转请求 resident owner 执行有页数和扫描上限的 direct reclaim。
///
/// # Parameters
///
/// - `limit`: 本轮最多需要释放的物理页数；零值不扫描。
///
/// # Returns
///
/// 所有 adapter 合计的实际释放与扫描页数。
pub(crate) fn reclaim_pages(limit: usize) -> ReclaimResult {
    reclaim(limit, ReclaimSource::Direct)
}

/// 后台执行体复用同一 registry/cursor，计数独立于 allocator stall。
pub(in crate::memory) fn reclaim_background_pages(limit: usize) -> ReclaimResult {
    reclaim(limit, ReclaimSource::Background)
}

fn reclaim(limit: usize, source: ReclaimSource) -> ReclaimResult {
    if limit == 0 {
        return ReclaimResult::default();
    }
    let request = ReclaimRequest::for_target(limit);
    let mut result = ReclaimResult::default();
    // 1. 冻结 stable-slot 数与 owner budget；每次最多访问 64 个 adapter，
    // 缺失上限会让单次用户缺页时延随全局 owner 数无界增长。
    let owner_budget = {
        let registry = reclaimer_registry().lock();
        registry.slots.len().min(64)
    };
    for _ in 0..owner_budget {
        if result.reclaimed_pages >= request.target_pages
            || result.scanned_pages >= request.scan_pages
        {
            break;
        }
        // 2. 只在 registry lock 内 clone 一个 Weak 并推进唯一 cursor；callback
        // 前释放锁，否则 AddressSpace/page-cache 锁会与注册路径形成反序。
        let owner = {
            let mut registry = reclaimer_registry().lock();
            let length = registry.slots.len();
            if length == 0 {
                None
            } else {
                let index = registry.cursor % length;
                registry.cursor = (index + 1) % length;
                registry.slots.get(&index).cloned()
            }
        };
        // 3. owner 可在 weak clone 后并发退出；跳过 tombstone，并把剩余的
        // 页目标和全局 scan budget 交给下一个 live adapter。
        if let Some(owner) = owner.and_then(|owner| owner.upgrade()) {
            let mut owner_request = ReclaimRequest::with_scan_budget(
                request.target_pages - result.reclaimed_pages,
                (request.scan_pages - result.scanned_pages).min(256),
            );
            owner_request.allow_swap_io = matches!(source, ReclaimSource::Background);
            let owner_result = owner.reclaim_pages(owner_request);
            assert!(
                owner_result.reclaimed_pages <= owner_request.target_pages
                    && owner_result.scanned_pages <= owner_request.scan_pages,
                "memory reclaimer exceeded its request"
            );
            result.reclaimed_pages += owner_result.reclaimed_pages;
            result.scanned_pages += owner_result.scanned_pages;
        }
    }
    let mut registry = reclaimer_registry().lock();
    let statistics = match source {
        ReclaimSource::Direct => &mut registry.statistics.direct,
        ReclaimSource::Background => &mut registry.statistics.background,
    };
    statistics.attempts = statistics.attempts.saturating_add(1);
    statistics.scanned_pages = statistics
        .scanned_pages
        .saturating_add(result.scanned_pages as u64);
    statistics.reclaimed_pages = statistics
        .reclaimed_pages
        .saturating_add(result.reclaimed_pages as u64);
    result
}

/// 返回 direct reclaim registry 的累计工作量。
///
/// # Returns
///
/// 调用、扫描和实际回收计数的同锁快照。
pub(crate) fn reclaim_statistics() -> ReclaimStatisticsSnapshot {
    reclaimer_registry().lock().statistics
}
