use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{Mutex, MutexGuard, Once};

use crate::fallible_tree::FallibleMap;
use crate::memory::{
    MemoryReclaimer, PAGE_SIZE, ReclaimRequest, ReclaimResult, SharedFileError, SharedFileId,
    SharedFileMapping, SharedFrame, SharedPage, invalidate_shared_file, register_memory_reclaimer,
};

use super::{FileSystemError, Inode, InodeType};

mod reclaim;
mod writeback;
mod writeback_batch;
use reclaim::CachedPages;

const PAGE_DIRTY: usize = 1 << (usize::BITS - 1);
const PAGE_WRITER_MASK: usize = PAGE_DIRTY - 1;

/// @description page-cache owner 在单次读取边界投影的全局页统计。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageCacheStatistics {
    /// 当前由 CachedFile 强拥有的 resident pages。
    pub(crate) resident_pages: usize,
    /// dirty bit 已发布且尚未完成 writeback 的 resident pages。
    pub(crate) dirty_pages: usize,
    /// 当前可由 allocator 慢路径直接释放的 clean、无外部引用 pages。
    pub(crate) reclaimable_pages: usize,
}

#[derive(Debug)]
pub(super) struct CachedPage {
    frame: SharedFrame,
    // OWNER: CachedPage 用一个 CAS domain 同时拥有 dirty bit 与 writable-PTE 引用计数。
    // 拆成两个 Atomic 会让 writeback 的 writer==0 / clear-dirty 间隙吞掉并发 writer publication。
    state: AtomicUsize,
}

impl CachedPage {
    fn dirty(&self) -> bool {
        self.state.load(Ordering::Acquire) & PAGE_DIRTY != 0
    }

    pub(super) fn reclaimable(&self) -> bool {
        self.state.load(Ordering::Acquire) == 0
    }

    fn mark_clean_if_unmapped(&self) {
        // writer acquire 与本 CAS 竞争：先 acquire 则 writer count 阻止清理，后 acquire
        // 则重新原子发布 dirty，因而不存在已映射 writable page 被误标 clean 的窗口。
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & PAGE_WRITER_MASK == 0).then_some(0)
            });
    }
}

impl SharedPage for CachedPage {
    fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    fn acquire_writer(&self) {
        let result = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & PAGE_WRITER_MASK)
                    .checked_add(1)
                    .filter(|writers| *writers <= PAGE_WRITER_MASK)
                    .map(|writers| PAGE_DIRTY | writers)
            });
        assert!(result.is_ok(), "shared-page writer count overflow");
    }

    fn release_writer(&self) {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let writers = state & PAGE_WRITER_MASK;
                (writers != 0).then_some((state & PAGE_DIRTY) | (writers - 1))
            })
            .expect("shared-page writer release without acquire");
    }
}

struct CachedFile {
    id: SharedFileId,
    inode: Arc<dyn Inode>,
    // OWNER: 单 inode operation lock 串行化 cache fill、write/append、truncate 与 writeback；
    // 缺失 fill 归属会让并发旧 storage read 在 write/truncate 后插入 stale cache page。
    operation: Mutex<()>,
    // OWNER: 单 inode write_sequence gate 排序完整 regular write、truncate、allocate 与 fd/global sync；
    // 缺失时 512-byte user-copy chunks 可被另一 OFD write 穿插，破坏 writev/append 连续性。
    // page fault 不获取该 gate，只取内层 operation，因此同文件 mmap buffer fault 不会自死锁。
    write_sequence: Mutex<()>,
    pages: Mutex<CachedPages>,
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
        _operation: &MutexGuard<'_, ()>,
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
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut pages = self.pages.lock();
        assert!(!pages.entries.contains_key(&index));
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
        let operation = self.operation.lock();
        self.page_after_operation_lock(index, &operation)
    }

    fn fault_page(&self, index: u64) -> Result<Arc<CachedPage>, FileSystemError> {
        // Fault 必须先与 truncate 串行化，即使 cache hit 也不能绕过稳定 EOF snapshot。
        let operation = self.operation.lock();
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
        let _operation = self.operation.lock();
        self.writeback_range(offset, length).map_err(fs_error)
    }
}

impl MemoryReclaimer for CachedFile {
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult {
        let Some(mut pages) = self.pages.try_lock() else {
            return ReclaimResult::default();
        };
        pages.reclaim(request)
    }
}

// OWNER: fs page-cache owns every regular inode cached page until clean reclaim. A weak-only map
// would lose dirty MAP_SHARED pages when the final VMA disappears before sync(2).
static FILES: Once<Mutex<FallibleMap<SharedFileId, Arc<CachedFile>>>> = Once::new();

fn cached_file(inode: Arc<dyn Inode>) -> Result<Arc<CachedFile>, FileSystemError> {
    if inode.inode_type() != InodeType::File || inode.is_volatile() {
        return Err(FileSystemError::InvalidOperation);
    }
    let id = SharedFileId {
        filesystem: inode.filesystem_id(),
        inode: inode.metadata()?.inode,
    };
    let mut files = FILES.call_once(|| Mutex::new(FallibleMap::new())).lock();
    if let Some(file) = files.get(&id) {
        return Ok(file.clone());
    }
    // reclaimer 注册成功后会持有 Arc；必须先预留 FILES node，避免注册完成却无法发布
    // 唯一 cache owner，形成不可回滚的孤儿 reclaimer。
    let file_slot = FallibleMap::<SharedFileId, Arc<CachedFile>>::try_reserve_node()
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let file = Arc::try_new(CachedFile {
        id,
        inode,
        operation: Mutex::new(()),
        write_sequence: Mutex::new(()),
        pages: Mutex::new(CachedPages::new()),
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    register_memory_reclaimer(file.clone()).map_err(shared_error)?;
    files.commit_vacant(file_slot.fill(id, file.clone()));
    Ok(file)
}

/// @description 持久 page-cache 与只读动态快照共用的 regular-file I/O facade。
///
/// syscall 在一次 read/write 操作内复用该值，避免每个 user-copy chunk 重复读取 inode
/// metadata、获取全局 FILES lock 并查找同一个 ordered entry。
pub(crate) struct RegularFile(RegularFileBackend);

enum RegularFileBackend {
    Cached(Arc<CachedFile>),
    Volatile(Arc<dyn Inode>),
}

/// @description 一次 regular-file cache read 的 logical 与实际 storage 结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegularFileRead {
    /// 复制到 kernel output 的 logical file bytes。
    pub(crate) bytes: usize,
    /// 本次调用因 cache miss 从 filesystem storage 成功填充的 bytes。
    pub(crate) storage_bytes: usize,
}

/// @description 持有单 inode write-sequence ownership 的一次 regular-file mutation。
///
/// Drop 无条件释放 gate；error、signal 或 partial user-copy 都不会遗留 transaction owner。
pub(crate) struct RegularFileWrite<'a> {
    file: &'a CachedFile,
    _sequence: MutexGuard<'a, ()>,
}

impl RegularFile {
    /// @description 将 regular inode 解析为持久 page-cache owner 或只读动态快照。
    /// @param inode 目标 regular inode。
    /// @return 可在本次 I/O 内复用的 facade；volatile inode 不注册全局 cache entry。
    /// @error `InvalidOperation` 表示 inode 不是 regular file。
    /// @error inode metadata 读取失败时透传 filesystem error。
    /// @error 首次注册 memory reclaimer 失败时返回 `OutOfMemory`。
    pub(crate) fn from_inode(inode: Arc<dyn Inode>) -> Result<Self, FileSystemError> {
        if inode.inode_type() != InodeType::File {
            return Err(FileSystemError::InvalidOperation);
        }
        if inode.is_volatile() {
            return Ok(Self(RegularFileBackend::Volatile(inode)));
        }
        cached_file(inode).map(|file| Self(RegularFileBackend::Cached(file)))
    }

    /// @description 返回持久文件的唯一 page-cache backing identity。
    /// @return 持久文件返回 filesystem/inode identity；动态快照没有 cache identity，返回 None。
    pub(crate) fn id(&self) -> Option<SharedFileId> {
        match &self.0 {
            RegularFileBackend::Cached(file) => Some(file.id),
            RegularFileBackend::Volatile(_) => None,
        }
    }

    /// @description 返回当前 regular-file byte size。
    /// @return filesystem metadata owner 的当前 i_size 投影。
    pub(crate) fn size(&self) -> u64 {
        match &self.0 {
            RegularFileBackend::Cached(file) => file.inode.size(),
            RegularFileBackend::Volatile(inode) => inode.size(),
        }
    }

    /// @description 从持久 page cache 或只读动态 inode 读取 regular-file bytes。
    /// @param offset 文件 byte offset。
    /// @param output kernel-owned 输出缓冲区。
    /// @return 实际读取字节数；EOF 返回零。
    /// @error cache fill 分配失败时返回 `OutOfMemory`。
    /// @error size snapshot 后并发 truncate 越过当前 page 时返回 `InvalidOperation`。
    /// @error storage read 失败或短读时返回对应 filesystem error。
    pub(crate) fn read(
        &self,
        offset: u64,
        output: &mut [u8],
    ) -> Result<RegularFileRead, FileSystemError> {
        let file = match &self.0 {
            RegularFileBackend::Cached(file) => file,
            RegularFileBackend::Volatile(inode) => {
                return inode
                    .read_storage(offset, output)
                    .map(|bytes| RegularFileRead {
                        bytes,
                        storage_bytes: 0,
                    });
            }
        };
        let size = file.inode.size();
        let count = usize::try_from(size.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(output.len());
        let mut done = 0;
        let mut storage_bytes = 0;
        while done < count {
            let current = offset + done as u64;
            let (page, filled) = file.page_with_storage(current / PAGE_SIZE as u64)?;
            storage_bytes += filled;
            let page_offset = current as usize % PAGE_SIZE;
            let part = (PAGE_SIZE - page_offset).min(count - done);
            page.frame.read(page_offset, &mut output[done..done + part]);
            done += part;
        }
        Ok(RegularFileRead {
            bytes: done,
            storage_bytes,
        })
    }

    /// @description 开始一次不可被其他 regular-file mutation 穿插的 write operation。
    /// @return 持有 per-inode write-sequence gate 的 mutation facade；Drop 自动释放。
    /// @error 只读动态 inode 返回 `ReadOnly`。
    pub(crate) fn begin_write(&self) -> Result<RegularFileWrite<'_>, FileSystemError> {
        let RegularFileBackend::Cached(file) = &self.0 else {
            return Err(FileSystemError::ReadOnly);
        };
        Ok(RegularFileWrite {
            file,
            _sequence: file.write_sequence.lock(),
        })
    }
}

impl RegularFileWrite<'_> {
    /// @description 向 regular-file storage 写入并同步更新 resident cache pages。
    /// @param offset 文件 byte offset。
    /// @param input kernel-owned 输入缓冲区。
    /// @return storage 实际写入字节数。
    /// @error storage mutation 失败时透传 filesystem error。
    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        let _operation = self.file.operation.lock();
        let written = self.file.inode.write_storage(offset, input)?;
        self.file.update_cached(offset, &input[..written]);
        Ok(written)
    }

    /// @description 在 page-cache operation lock 内原子执行受最大文件大小约束的 append。
    /// @param input 待追加数据。
    /// @param size_limit caller 的 RLIMIT_FSIZE soft limit。
    /// @return append 起始 offset 与实际字节数；已到上限时返回零字节，由 syscall 生成 SIGXFSZ/EFBIG。
    /// @error storage mutation 失败时透传 filesystem error。
    pub(crate) fn append(
        &self,
        input: &[u8],
        size_limit: u64,
    ) -> Result<(u64, usize), FileSystemError> {
        let _operation = self.file.operation.lock();
        let offset = self.file.inode.size();
        let allowed = usize::try_from(size_limit.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(input.len());
        if allowed == 0 {
            return Ok((offset, 0));
        }
        let (offset, written) = self.file.inode.append_storage(&input[..allowed])?;
        self.file.update_cached(offset, &input[..written]);
        Ok((offset, written))
    }
}

pub(crate) fn mapping(
    inode: Arc<dyn Inode>,
) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
    cached_file(inode).map(|file| file as Arc<dyn SharedFileMapping>)
}

pub(crate) fn truncate(inode: Arc<dyn Inode>, size: u64) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.truncate_storage(size);
    }
    let file = cached_file(inode)?;
    let _sequence = file.write_sequence.lock();
    let _operation = file.operation.lock();
    file.inode.truncate_storage(size)?;
    let first_removed = size.div_ceil(PAGE_SIZE as u64);
    let mut pages = file.pages.lock();
    pages.entries.retain(|index, _| *index < first_removed);
    if !size.is_multiple_of(PAGE_SIZE as u64)
        && let Some(page) = pages.entries.get(&(size / PAGE_SIZE as u64))
    {
        page.frame.zero_from(size as usize % PAGE_SIZE);
    }
    drop(pages);
    drop(_operation);
    invalidate_shared_file(file.id, size);
    Ok(())
}

/// @description 在 page-cache operation domain 内预分配 regular-file backing blocks。
/// @param inode 目标 regular inode。
/// @param offset byte range 起点。
/// @param length 非零 byte range 长度。
/// @return allocation 与可能的 size extension 完成；cached contents 保持不变。
pub(crate) fn allocate(
    inode: Arc<dyn Inode>,
    offset: u64,
    length: u64,
) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.allocate_storage(offset, length);
    }
    let file = cached_file(inode)?;
    let _sequence = file.write_sequence.lock();
    let _operation = file.operation.lock();
    file.inode.allocate_storage(offset, length)
}

pub(crate) fn sync_inode(inode: Arc<dyn Inode>) -> Result<(), FileSystemError> {
    if inode.inode_type() != InodeType::File {
        return inode.sync_storage();
    }
    let file = cached_file(inode)?;
    let _sequence = file.write_sequence.lock();
    let _operation = file.operation.lock();
    file.writeback_range(0, u64::MAX)
}

pub(crate) fn sync_all() -> Result<(), FileSystemError> {
    let registry = FILES.call_once(|| Mutex::new(FallibleMap::new())).lock();
    let mut files = Vec::new();
    files
        .try_reserve_exact(registry.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    files.extend(registry.values().cloned());
    drop(registry);
    for file in files {
        let _sequence = file.write_sequence.lock();
        let _operation = file.operation.lock();
        file.writeback_range(0, u64::MAX)?;
    }
    FILES
        .wait()
        .lock()
        .retain(|_, file| Arc::strong_count(file) != 1 || !file.pages.lock().entries.is_empty());
    Ok(())
}

/// @description 从唯一 CachedFile page maps 汇总一次全局 resident/dirty/reclaimable 快照。
///
/// @return 只读统计；不触发 fill、writeback 或 reclaim。
pub(crate) fn statistics() -> PageCacheStatistics {
    let files = FILES.call_once(|| Mutex::new(FallibleMap::new())).lock();
    let mut statistics = PageCacheStatistics {
        resident_pages: 0,
        dirty_pages: 0,
        reclaimable_pages: 0,
    };
    for file in files.values() {
        let pages = file.pages.lock();
        statistics.resident_pages += pages.entries.len();
        for page in pages.entries.values() {
            statistics.dirty_pages += usize::from(page.dirty());
            statistics.reclaimable_pages +=
                usize::from(page.reclaimable() && Arc::strong_count(page) == 1);
        }
    }
    statistics
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
