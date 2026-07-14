use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{fmt::Debug, ptr};
use spin::{Mutex, Once};

use super::{address::PhysicalPageNumber, config::PAGE_SIZE, frame_allocator};

/// @description 跨 fs/memory seam 标识一个 mounted filesystem inode。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SharedFileId {
    pub(crate) filesystem: usize,
    pub(crate) inode: u64,
}

/// @description page-cache 获取共享页时的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedFileError {
    OutOfMemory,
    Io,
    BeyondEof,
}

/// @description 可同时被用户页表与 kernel page cache 引用的单页物理存储。
#[derive(Debug)]
pub(crate) struct SharedFrame {
    frame: frame_allocator::FrameTracker,
}

impl SharedFrame {
    /// @description 分配并清零一个共享物理页。
    /// @return 成功返回唯一 owner；物理内存耗尽返回 OutOfMemory。
    pub(crate) fn allocate() -> Result<Self, SharedFileError> {
        frame_allocator::alloc()
            .map(|frame| Self { frame })
            .ok_or(SharedFileError::OutOfMemory)
    }

    /// @description 返回页表映射使用的物理页号。
    pub(crate) fn ppn(&self) -> PhysicalPageNumber {
        self.frame.ppn
    }

    /// @description 独占借用尚未发布的共享页内容，供 page-cache miss 直接填充 storage bytes。
    /// @return 生命周期绑定到 `SharedFrame` 独占借用的完整页切片。
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        self.frame.bytes_mut()
    }

    /// @description 从共享页复制到 kernel buffer。
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

    /// @description 从 kernel buffer 写入共享页并立即对现有 PTE 可见。
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

    /// @description 将页内指定偏移到页尾清零，供 truncate 隐藏旧 EOF 尾部数据。
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

/// @description MemorySet 持有的共享 cache page interface。
pub(crate) trait SharedPage: Send + Sync + Debug {
    fn frame(&self) -> &SharedFrame;
    fn acquire_writer(&self);
    fn release_writer(&self);
}

/// @description file-backed shared VMA 消费的 page-cache interface。
pub(crate) trait SharedFileMapping: Send + Sync + Debug {
    fn id(&self) -> SharedFileId;
    fn size(&self) -> u64;
    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError>;
    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError>;
}

/// @description memory subsystem 对 live AddressSpace 的反向维护 interface。
pub(crate) trait MemoryMappingOwner: Send + Sync {
    fn invalidate_shared_file(&self, id: SharedFileId, size: u64);
}

/// @description 一次 direct-reclaim adapter 调用的页目标与扫描上限。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReclaimRequest {
    target_pages: usize,
    scan_pages: usize,
}

impl ReclaimRequest {
    /// @description 为物理页目标建立固定放大率的扫描预算。
    ///
    /// @param target_pages 本轮最多需要释放的物理页数。
    /// @return 扫描预算至少覆盖 256 个 resident entry，且不会发生整数溢出。
    pub(crate) fn for_target(target_pages: usize) -> Self {
        Self {
            target_pages,
            scan_pages: target_pages.saturating_mul(16).max(256),
        }
    }

    fn with_scan_budget(target_pages: usize, scan_pages: usize) -> Self {
        Self {
            target_pages,
            scan_pages,
        }
    }

    /// @description 返回本 adapter 最多需要释放的物理页数。
    pub(crate) fn target_pages(self) -> usize {
        self.target_pages
    }

    /// @description 返回本 adapter 最多可检查的 resident entry 数。
    pub(crate) fn scan_pages(self) -> usize {
        self.scan_pages
    }
}

/// @description 一次 direct-reclaim adapter 调用的实际工作量。
#[must_use = "reclaim progress and scan cost must be propagated to the caller"]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReclaimResult {
    reclaimed_pages: usize,
    scanned_pages: usize,
}

/// @description direct reclaim 唯一 registry owner 的累计工作量。
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ReclaimStatistics {
    /// 非零页目标的 direct-reclaim 调用数。
    pub(crate) attempts: u64,
    /// 所有 adapter 实际检查的 resident entry 数。
    pub(crate) scanned_pages: u64,
    /// 实际归还 frame allocator 的物理页数。
    pub(crate) reclaimed_pages: u64,
}

impl ReclaimResult {
    /// @description 构造 adapter 的实际回收结果。
    ///
    /// @param reclaimed_pages 已释放回 frame allocator 的物理页数。
    /// @param scanned_pages 已检查的 resident entry 数。
    /// @return 保留两项独立计数的结果；撤销共享 frame 的单个映射不会伪报物理页释放。
    pub(crate) const fn new(reclaimed_pages: usize, scanned_pages: usize) -> Self {
        Self {
            reclaimed_pages,
            scanned_pages,
        }
    }
}

/// @description frame allocator 慢路径使用的物理页回收 seam；具体 owner 不泄漏到 memory 下层。
pub(crate) trait MemoryReclaimer: Send + Sync {
    /// @description 在 adapter 自己的 owner lock 下执行一次有界 direct reclaim。
    ///
    /// @param request 需要释放的页目标与允许检查的 resident entry 上限。
    /// @return 实际释放和扫描的页数；两项都不得超过 request 对应上限。
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult;
}

// OWNER: memory module owns stable weak address-space invalidation slots. Holding strong Process
// references here would keep exited address spaces and every mapped page alive forever; removing
// dead slots would shift a concurrent truncate scan and leave a live AddressSpace uninformed.
static MEMORY_MAPPING_OWNERS: Once<Mutex<Vec<Weak<dyn MemoryMappingOwner>>>> = Once::new();
struct ReclaimerRegistry {
    slots: Vec<Weak<dyn MemoryReclaimer>>,
    // OWNER: cursor 与 stable weak slots 由同一 registry lock 拥有，只表示下一个
    // direct-reclaim adapter。缺失它会让达到 quota 的短扫描每次都从首 slot
    // 开始，长期饿死后续 AddressSpace/page-cache owner。
    cursor: usize,
    // OWNER: statistics 是同一 registry lock 下对已完成 reclaim transaction 的
    // 只读 projection。缺失单一提交点会让 pgscan/pgsteal 跨 owner 重复计数。
    statistics: ReclaimStatistics,
}

// OWNER: memory module 只保存 weak reclaimer adapter 与单一轮转游标；强引用会让
// 已退出 AddressSpace 或已移除 page-cache object 永久存活。callback 前释放
// registry lock，避免 owner 锁序反转。
static MEMORY_RECLAIMERS: Once<Mutex<ReclaimerRegistry>> = Once::new();

fn reclaimer_registry() -> &'static Mutex<ReclaimerRegistry> {
    MEMORY_RECLAIMERS.call_once(|| {
        Mutex::new(ReclaimerRegistry {
            slots: Vec::new(),
            cursor: 0,
            statistics: ReclaimStatistics::default(),
        })
    })
}

/// @description 注册一个 address-space invalidator，registry 只保留 weak lifetime。
pub(crate) fn register_memory_mapping_owner(
    owner: Arc<dyn MemoryMappingOwner>,
) -> Result<(), SharedFileError> {
    let mut owners = MEMORY_MAPPING_OWNERS
        .call_once(|| Mutex::new(Vec::new()))
        .lock();
    // Dead weak owners are reusable capacity. Appending every historical AddressSpace would make
    // the first later truncate scan exited processes and could report ENOMEM despite a free slot.
    if let Some(slot) = owners.iter_mut().find(|slot| slot.strong_count() == 0) {
        *slot = Arc::downgrade(&owner);
        return Ok(());
    }
    owners
        .try_reserve(1)
        .map_err(|_| SharedFileError::OutOfMemory)?;
    owners.push(Arc::downgrade(&owner));
    Ok(())
}

/// @description 注册一个只保留 weak lifetime 的物理页回收 owner。
pub(crate) fn register_memory_reclaimer(
    owner: Arc<dyn MemoryReclaimer>,
) -> Result<(), SharedFileError> {
    let mut registry = reclaimer_registry().lock();
    // Dead weak slots are stable tombstones until registration reuses them. Removing slots would
    // shift a concurrent OOM scan's cursor and silently skip a still-live reclaimer.
    if let Some(slot) = registry
        .slots
        .iter_mut()
        .find(|slot| slot.strong_count() == 0)
    {
        *slot = Arc::downgrade(&owner);
        return Ok(());
    }
    registry
        .slots
        .try_reserve(1)
        .map_err(|_| SharedFileError::OutOfMemory)?;
    registry.slots.push(Arc::downgrade(&owner));
    Ok(())
}

/// @description 在不分配内存且不持有 registry lock 回调的前提下撤销所有 EOF 外 PTE。
/// @param id 已完成 storage truncate 的 mounted inode identity。
/// @param size 已提交的新文件字节长度。
/// @return 所有本轮可见的 live AddressSpace 均已完成 invalidation。
pub(crate) fn invalidate_shared_file(id: SharedFileId, size: u64) {
    let registry = MEMORY_MAPPING_OWNERS.call_once(|| Mutex::new(Vec::new()));
    // 1. 固定本轮起点时已发布的 slot 数；随后注册的 AddressSpace 尚未拥有旧 EOF 映射。
    let slot_count = registry.lock().len();
    for index in 0..slot_count {
        // 2. 每次只在 registry lock 内 clone 一个 Weak。若建立 live Vec，truncate 已提交后
        // 仍可能因 snapshot OOM 返回，并永久留下 EOF 外 stale PTE。
        let owner = registry.lock().get(index).cloned();
        // 3. callback 前释放 registry lock，避免长时间 TLB shootdown 阻塞新 mm publication；
        // owner 在 clone 后退出是安全的，已经销毁的页表不再需要 invalidation。
        if let Some(owner) = owner.and_then(|owner| owner.upgrade()) {
            owner.invalidate_shared_file(id, size);
        }
    }
}

/// @description 轮转请求 resident owner 执行有页数和扫描上限的 direct reclaim。
///
/// @param limit 本轮最多需要释放的物理页数；零值不扫描。
/// @return 所有 adapter 合计的实际释放与扫描页数。
pub(crate) fn reclaim_pages(limit: usize) -> ReclaimResult {
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
                registry.slots.get(index).cloned()
            }
        };
        // 3. owner 可在 weak clone 后并发退出；跳过 tombstone，并把剩余的
        // 页目标和全局 scan budget 交给下一个 live adapter。
        if let Some(owner) = owner.and_then(|owner| owner.upgrade()) {
            let owner_request = ReclaimRequest::with_scan_budget(
                request.target_pages - result.reclaimed_pages,
                (request.scan_pages - result.scanned_pages).min(256),
            );
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
    registry.statistics.attempts = registry.statistics.attempts.saturating_add(1);
    registry.statistics.scanned_pages = registry
        .statistics
        .scanned_pages
        .saturating_add(result.scanned_pages as u64);
    registry.statistics.reclaimed_pages = registry
        .statistics
        .reclaimed_pages
        .saturating_add(result.reclaimed_pages as u64);
    result
}

/// @description 返回 direct reclaim registry 的累计工作量。
///
/// @return 调用、扫描和实际回收计数的同锁快照。
pub(crate) fn reclaim_statistics() -> ReclaimStatistics {
    reclaimer_registry().lock().statistics
}
