use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Mutex;

use crate::fallible_tree::FallibleMap;
use crate::memory::{
    MemoryReclaimer, PAGE_SIZE, ReclaimRequest, ReclaimResult, SharedFileError, SharedFileId,
    SharedFileMapping, SharedFrame, SharedPage, SharedPagePin, invalidate_shared_file,
    register_memory_reclaimer,
};
use crate::sync::{TaskEvent, TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation};

use super::AccessIdentity;
use super::CompletionNotify;
use super::memory_file::{MemoryFile, MemoryWrite};
use super::{DataBacking, FileSystemError, Inode, InodeType};

mod nowait_read;
mod storage_epoch;
use storage_epoch::StorageEpoch;
mod regular_file;
mod swap;
pub(crate) use nowait_read::{NowaitRead, ReadStep};
pub(crate) use regular_file::RegularFile;
pub(super) use swap::prepare_swap_file;
mod registry;
use registry::cached_file;
pub(super) use registry::evict_if_unlinked;
pub(crate) use registry::{evict_cached, evict_filesystem, statistics, sync_all, writeback_cached};
mod reclaim;
mod regular_write;
mod writeback;
mod writeback_batch;
use reclaim::CachedPages;
use writeback_batch::WRITEBACK_BATCH_PAGES;

const PAGE_DIRTY: usize = 1 << (usize::BITS - 1);
const PAGE_WRITER_MASK: usize = PAGE_DIRTY - 1;
const DIRTY_LIMIT_DIVISOR: usize = 4;
const DIRTY_THROTTLE_PAGES: usize = WRITEBACK_BATCH_PAGES * 2;

// OWNER: dirty count 只投影 CachedPage 单一 atomic state 的 clean→dirty / dirty→clean
// 转换。缺少该高水位投影会让 writable mapping 吃完可用于 journal writeback 的内存。
static DIRTY_PAGES: AtomicUsize = AtomicUsize::new(0);
// OWNER: 此 gate 只合并跨 CPU 的同一 dirty 高水位回收尝试；缺失它会让多个
// writable fault 同时扫描相同 owner，放大 CPU/FLUSH，而跳过者仍由后续 cadence 重试。
static DIRTY_THROTTLE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// page-cache owner 在单次读取边界投影的全局页统计。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageCacheStatistics {
    /// 当前由 CachedFile 强拥有的 resident pages。
    pub(crate) resident_pages: usize,
    /// dirty bit 已发布且尚未完成 writeback 的 resident pages。
    pub(crate) dirty_pages: usize,
    /// 当前可由 allocator 慢路径直接释放的 clean、无外部引用 pages。
    pub(crate) reclaimable_pages: usize,
}

pub(super) struct CachedPage {
    frame: SharedFrame,
    // OWNER: CachedPage 用一个 CAS domain 同时拥有 dirty bit 与 writable-PTE 引用计数。
    // 拆成两个 Atomic 会让 writeback 的 writer==0 / clear-dirty 间隙吞掉并发 writer publication。
    state: AtomicUsize,
    writer_retired: Arc<TaskEvent>,
}

impl core::fmt::Debug for CachedPage {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CachedPage")
            .field("frame", &self.frame)
            .finish_non_exhaustive()
    }
}

impl CachedPage {
    fn dirty(&self) -> bool {
        self.state.load(Ordering::Acquire) & PAGE_DIRTY != 0
    }

    pub(super) fn reclaimable(&self) -> bool {
        self.state.load(Ordering::Acquire) == 0
    }

    fn dirty_unmapped(&self) -> bool {
        self.state.load(Ordering::Acquire) == PAGE_DIRTY
    }

    fn mark_clean_if_unmapped(&self) {
        // writer acquire 与本 CAS 竞争：先 acquire 则 writer count 阻止清理，后 acquire
        // 则重新原子发布 dirty，因而不存在已映射 writable page 被误标 clean 的窗口。
        if let Ok(previous) = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & PAGE_WRITER_MASK == 0).then_some(0)
            })
            && previous & PAGE_DIRTY != 0
        {
            DIRTY_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for CachedPage {
    fn drop(&mut self) {
        if self.dirty() {
            DIRTY_PAGES.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl SharedPage for CachedPage {
    fn pin(&self) -> Result<SharedPagePin<'_>, SharedFileError> {
        Ok(SharedPagePin::Cached(&self.frame))
    }

    fn acquire_writer(&self) {
        // 在发布新 writable PTE 前提前回写旧的 unmapped dirty 页。高水位只影响慢路径，
        // 且此时仍保留 3/4 物理容量供 ext4 journal staging，避免 OOM 后递归回写自锁。
        let dirty_limit = crate::memory::frame_statistics()
            .capacity_pages
            .div_ceil(DIRTY_LIMIT_DIVISOR);
        let dirty = DIRTY_PAGES.load(Ordering::Relaxed);
        if !self.dirty()
            && dirty >= dirty_limit
            && (dirty - dirty_limit).is_multiple_of(DIRTY_THROTTLE_PAGES)
            && DIRTY_THROTTLE_ACTIVE
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            let _ = crate::memory::reclaim_pages(DIRTY_THROTTLE_PAGES);
            DIRTY_THROTTLE_ACTIVE.store(false, Ordering::Release);
        }
        let result = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & PAGE_WRITER_MASK)
                    .checked_add(1)
                    .filter(|writers| *writers <= PAGE_WRITER_MASK)
                    .map(|writers| PAGE_DIRTY | writers)
            });
        let previous = result.expect("shared-page writer count overflow");
        if previous & PAGE_DIRTY == 0 {
            DIRTY_PAGES.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release_writer(&self) {
        let previous = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let writers = state & PAGE_WRITER_MASK;
                (writers != 0).then_some((state & PAGE_DIRTY) | (writers - 1))
            })
            .expect("shared-page writer release without acquire");
        if previous & PAGE_WRITER_MASK == 1 {
            self.writer_retired.signal();
        }
    }
}

struct CachedFile {
    id: SharedFileId,
    inode: Arc<dyn Inode>,
    // OWNER: 单 inode operation lock 串行化 cache fill、write/append、truncate 与 writeback；
    // 缺失 fill 归属会让并发旧 storage read 在 write/truncate 后插入 stale cache page。
    operation: TaskMutex<()>,
    // OWNER: 单 inode write_sequence gate 排序完整 regular write、truncate、allocate 与 fd/global sync；
    // 缺失时 512-byte user-copy chunks 可被另一 OFD write 穿插，破坏 writev/append 连续性。
    // page fault 不获取该 gate，只取内层 operation，因此同文件 mmap buffer fault 不会自死锁。
    write_sequence: TaskMutex<()>,
    pages: Mutex<CachedPages>,
    // OWNER: seqlock 式的存储版本：每次改变文件内容或 EOF 的 operation（write/append、truncate、allocate）
    // 在持有 `operation` 期间使其为奇数，结束后再加一变回偶数。不持 `operation` 的异步填页据此丢弃
    // 读期间被改写的数据；缺失时它会把截断/改写之前读到的旧块插入 cache，之后一直被当作文件内容。
    storage_epoch: StorageEpoch,
    // OWNER: one preallocated event for freeze drain; page claims signal their last writer retirement.
    // Exiting mm may already have lost its Weak identity while its post-fence fields still own writers.
    writer_retired: Arc<TaskEvent>,
}

impl core::fmt::Debug for CachedFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CachedFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl CachedFile {
    fn page_after_operation_lock(
        &self,
        index: u64,
        _operation: &TaskMutexGuard<'_, ()>,
    ) -> Result<(Arc<CachedPage>, usize), FileSystemError> {
        // EOF 必须在任何 node/frame allocation 前、与 truncate 同一 operation domain 内判定。
        // private/shared fault 以返回的 Arc 作为 fault-before-truncate 的瞬时线性化凭据。
        let offset = index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let size = self.inode.size();
        if offset >= size {
            return Err(FileSystemError::InvalidOperation);
        }
        if let Some(page) = self.pages.lock().entries.get(&index).cloned() {
            return Ok((page, 0));
        }
        // 在 frame/I/O 等外部资源进入事务前预留 cache membership；缺失该步骤会在
        //    storage read 成功后因 tree node OOM 直接 panic，且无法向 fault/read 返回 ENOMEM。
        let page_slot = FallibleMap::<u64, Arc<CachedPage>>::try_reserve_node()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut frame = SharedFrame::allocate().map_err(shared_error)?;
        let available = usize::try_from(size - offset)
            .unwrap_or(usize::MAX)
            .min(PAGE_SIZE);
        // frame 尚未发布且保持独占，storage 直接填充其有效前缀；临时 Vec 会在每次
        // cache miss 增加一次 heap allocation 和一次最多整页 memcpy。
        let read = self
            .inode
            .read_storage(offset, &mut frame.bytes_mut()[..available])?;
        if read != available {
            return Err(FileSystemError::IoError);
        }
        let page = Arc::try_new(CachedPage {
            frame,
            state: AtomicUsize::new(0),
            writer_retired: self.writer_retired.clone(),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut pages = self.pages.lock();
        // 不持 `operation` 的异步填页可能已经发布了同一页；它读到的是同一份未被改写的数据（改写者
        // 都在 `operation` 里，而我们现在持有它），所以直接用已发布的页。
        if let Some(existing) = pages.entries.get(&index).cloned() {
            return Ok((existing, 0));
        }
        pages
            .entries
            .commit_vacant(page_slot.fill(index, page.clone()));
        Ok((page, available))
    }

    fn page_with_storage(&self, index: u64) -> Result<(Arc<CachedPage>, usize), FileSystemError> {
        if let Some(page) = self.pages.lock().entries.get(&index).cloned() {
            return Ok((page, 0));
        }
        // Regular read 保留 cache-hit fast path；miss 与 storage mutation 共用 operation domain。
        let operation = self
            .operation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.page_after_operation_lock(index, &operation)
    }

    fn fault_page(&self, index: u64) -> Result<Arc<CachedPage>, FileSystemError> {
        // Fault 必须先与 truncate 串行化，即使 cache hit 也不能绕过稳定 EOF snapshot。
        let operation = self
            .operation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.page_after_operation_lock(index, &operation)
            .map(|(page, _)| page)
    }

    fn update_cached(&self, offset: u64, input: &[u8]) {
        if input.is_empty() {
            return;
        }
        // 1. storage adapter 已确认完整写区间；overflow 表示其返回值破坏内部契约，必须 fail-stop。
        let end = offset
            .checked_add(input.len() as u64)
            .expect("storage write returned an overflowing byte range");
        let first = offset / PAGE_SIZE as u64;
        let last = (end - 1) / PAGE_SIZE as u64;
        let pages = self.pages.lock();
        // 2. 从 page index 反向遍历实际 resident pages，跳过大写入区间中的 cache holes。
        for (&index, page) in pages
            .entries
            .iter_from(&first)
            .take_while(|(index, _)| **index <= last)
        {
            let page_start = index * PAGE_SIZE as u64;
            let copy_start = offset.max(page_start);
            let copy_end = end.min(page_start.saturating_add(PAGE_SIZE as u64));
            // 3. 首尾 page 只覆盖交集；中间 page 自然复制完整 PAGE_SIZE。
            let source_start = (copy_start - offset) as usize;
            let page_offset = (copy_start - page_start) as usize;
            let count = (copy_end - copy_start) as usize;
            page.frame
                .write(page_offset, &input[source_start..source_start + count]);
        }
    }
}

impl SharedFileMapping for CachedFile {
    fn writable(&self) -> bool {
        !self.inode.is_swap_file()
    }
    fn id(&self) -> SharedFileId {
        self.id
    }

    fn size(&self) -> u64 {
        self.inode.size()
    }

    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError> {
        self.fault_page(index)
            .map(|page| page as Arc<dyn SharedPage>)
            .map_err(fs_error)
    }

    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError> {
        // AddressSpace owner 在调用 mapping sync 时已持 mm lock；这里只取内层 operation，
        // 否则会与 write_sequence → user-copy(AddressSpace) 形成反向锁序。
        let _operation = self
            .operation
            .lock()
            .map_err(|_| SharedFileError::OutOfMemory)?;
        self.writeback_range(offset, length).map_err(fs_error)
    }
}

impl MemoryReclaimer for CachedFile {
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult {
        self.reclaim_under_pressure(request)
    }
}

/// 持有单 inode write-sequence ownership 的一次 regular-file mutation。
///
/// Drop 无条件释放 gate；error、signal 或 partial user-copy 都不会遗留 transaction owner。
pub(crate) enum RegularFileWrite<'a> {
    Cached(CachedWrite<'a>),
    Memory(MemoryInodeWrite<'a>),
}

/// page-cache 文件的一次 write 序列化权。
pub(crate) struct CachedWrite<'a> {
    file: &'a CachedFile,
    identity: AccessIdentity,
    _sequence: TaskMutexGuard<'a, ()>,
}

/// 同一 write sequence 下保持 inode metadata owner 与 MemoryFile 内容 owner。
pub(crate) struct MemoryInodeWrite<'a> {
    write: MemoryWrite<'a>,
    inode: &'a Arc<dyn Inode>,
    identity: AccessIdentity,
}

/// 建立 file-backed mmap 的共享映射。
///
/// # Parameters
///
/// - `inode`: 被映射的 regular file。
/// - `owner`: 映射来源的实际 file object（Linux `vma->vm_file`）；保留其所有 lease 与 close
///   lifecycle。只有真实的内部无 fd backing 使用 None，memfd 的用户映射也必须保留 OFD。
///
/// # Errors
///
/// 非 regular file 返回 `InvalidOperation`；分配失败返回 `OutOfMemory`。
pub(crate) fn mapping(
    inode: Arc<dyn Inode>,
    owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
    may_write: bool,
) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return Err(FileSystemError::InvalidOperation);
    }
    let inner: Arc<dyn SharedFileMapping> = match inode.data_backing() {
        DataBacking::PageCache => cached_file(inode)?.ok_or(FileSystemError::InvalidOperation)?,
        DataBacking::Memory(file) => file.mapping()?,
        DataBacking::Snapshot => return Err(FileSystemError::InvalidOperation),
    };
    Arc::try_new(MappedFile {
        inner,
        _owner: owner,
        may_write,
    })
    .map(|mapping| mapping as Arc<dyn SharedFileMapping>)
    .map_err(|_| FileSystemError::OutOfMemory)
}

/// 一个 mmap 数据视图，同时 pin 住实际 file object；不能只保留 dentry 而丢失 writer/backend。
struct MappedFile {
    inner: Arc<dyn SharedFileMapping>,
    _owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
    may_write: bool,
}

impl core::fmt::Debug for MappedFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MappedFile")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl SharedFileMapping for MappedFile {
    fn writable(&self) -> bool {
        self.inner.writable()
    }
    fn may_write(&self) -> bool {
        self.may_write
    }
    fn id(&self) -> SharedFileId {
        self.inner.id()
    }

    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError> {
        self.inner.page(index)
    }

    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError> {
        self.inner.sync_range(offset, length)
    }
}

pub(crate) fn truncate(
    inode: Arc<dyn Inode>,
    size: u64,
    identity: &AccessIdentity,
) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.truncate_storage(size, Some(identity));
    }
    let file = match inode.data_backing() {
        DataBacking::PageCache => cached_file(inode)?.ok_or(FileSystemError::InvalidOperation)?,
        DataBacking::Memory(_) => return inode.truncate_storage(size, Some(identity)),
        DataBacking::Snapshot => return inode.truncate_storage(size, Some(identity)),
    };
    // Post-storage PTE invalidation 不可回滚；在修改 inode/cache 前预分配唯一 waiter，
    // 使遍历 live AddressSpace 时只阻塞、不再产生 OOM failure window。
    let mut invalidation_wait =
        TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
    let _sequence = file
        .write_sequence
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _operation = file
        .operation
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _epoch = file.begin_mutation();
    file.inode.truncate_storage(size, Some(identity))?;
    let first_removed = size.div_ceil(PAGE_SIZE as u64);
    let mut pages = file.pages.lock();
    pages.entries.retain(|index, _| *index < first_removed);
    if !size.is_multiple_of(PAGE_SIZE as u64)
        && let Some(page) = pages.entries.get(&(size / PAGE_SIZE as u64))
    {
        page.frame.zero_from(size as usize % PAGE_SIZE);
    }
    drop(pages);
    drop(_epoch);
    drop(_operation);
    invalidate_shared_file(file.id, size, &mut invalidation_wait);
    Ok(())
}

/// 在 page-cache operation domain 内预分配 regular-file backing blocks。
///
/// # Parameters
///
/// - `inode`: 目标 regular inode。
/// - `offset`: byte range 起点。
/// - `length`: 非零 byte range 长度。
///
/// # Returns
///
/// allocation 与可能的 size extension 完成；cached contents 保持不变。
pub(crate) fn allocate(
    inode: Arc<dyn Inode>,
    offset: u64,
    length: u64,
    identity: &AccessIdentity,
) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.allocate_storage(offset, length, Some(identity));
    }
    let file = match inode.data_backing() {
        DataBacking::PageCache => cached_file(inode)?.ok_or(FileSystemError::InvalidOperation)?,
        DataBacking::Memory(_) => return inode.allocate_storage(offset, length, Some(identity)),
        DataBacking::Snapshot => return Err(FileSystemError::ReadOnly),
    };
    let _sequence = file
        .write_sequence
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _operation = file
        .operation
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _epoch = file.begin_mutation();
    file.inode.allocate_storage(offset, length, Some(identity))
}

pub(crate) fn sync_inode(inode: Arc<dyn Inode>) -> Result<(), FileSystemError> {
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return inode.sync_storage();
    }
    let block_device = inode.inode_type() == InodeType::BlockDevice;
    let file = match inode.data_backing() {
        DataBacking::PageCache => cached_file(inode)?.ok_or(FileSystemError::InvalidOperation)?,
        // 内存型文件没有 backing storage，也不在 page cache 里，无事可同步。
        DataBacking::Memory(_) | DataBacking::Snapshot => return Ok(()),
    };
    let _sequence = file
        .write_sequence
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let _operation = file
        .operation
        .lock()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    file.writeback_range(0, u64::MAX)?;
    if block_device {
        // 块设备的“数据已写回”只到设备队列；fsync 还要求设备把它推进到稳定存储。
        file.inode.sync_storage()?;
    }
    Ok(())
}

fn fs_error(error: FileSystemError) -> SharedFileError {
    match error {
        FileSystemError::OutOfMemory => SharedFileError::OutOfMemory,
        FileSystemError::InvalidOperation => SharedFileError::BeyondEof,
        _ => SharedFileError::Io,
    }
}

fn shared_error(error: SharedFileError) -> FileSystemError {
    match error {
        SharedFileError::OutOfMemory => FileSystemError::OutOfMemory,
        SharedFileError::BeyondEof => FileSystemError::InvalidOperation,
        SharedFileError::Io => FileSystemError::IoError,
    }
}
