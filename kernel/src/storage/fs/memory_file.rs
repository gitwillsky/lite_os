//! 内存型 regular file 的唯一存储：页即内容，单份，生命周期跟随持有它的 inode。
//!
//! tmpfs 与 memfd 共用。与磁盘文件不同，这里没有 backing storage，因此不经全局 page cache：
//! 页缓存的 registry 会让已删除文件的 inode（连同内容）一直存活到下一次 `sync`，并让热数据
//! 占两份内存。读写、mmap 与 truncate 都直接作用于这些页；页只在文件被删除且没有 open/mmap
//! 引用时随 `Arc` 释放，并立即归还配额。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::FileSystemError;
use crate::memory::{
    PAGE_SIZE, SharedFileError, SharedFileId, SharedFileMapping, SharedFrame, SharedPage,
    invalidate_shared_file,
};
use crate::sync::{TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation};

#[path = "memory_file/seals.rs"]
mod seals;
#[path = "memory_file/sparse.rs"]
mod sparse;
use seals::{SealError, Seals};
use sparse::{PageBytes, SparsePages, Stop};

const _: () = assert!(sparse::PAGE_SIZE == PAGE_SIZE);

fn touch(modified_ns: &AtomicU64) {
    modified_ns.store(crate::timer::get_realtime_ns(), Ordering::Relaxed);
}

/// [`PageBudget`] 的“不限”哨兵。
const UNLIMITED: u64 = u64::MAX;

/// 一个 filesystem 实例可用的页配额；所有属于它的 [`MemoryFile`] 共享同一个。
pub(crate) struct PageBudget {
    // 页数上限，`UNLIMITED` 表示只受物理内存限制（memfd）。可经 tmpfs `remount` 调整，所以是原子量；
    // 预留与调整的竞争由 `try_reserve` 的 CAS 与 `set_limit` 的 CAS 共同裁决。
    limit: AtomicU64,
    // OWNER: 已被该配额下的存储页占用的页数；页在创建时预留、在最后一个 `Arc` 释放时归还。
    // 缺失原子预留会让并发写入一起越过 `size=` 上限。
    used: AtomicU64,
}

impl PageBudget {
    pub(crate) fn new(limit: Option<u64>) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            limit: AtomicU64::new(limit.unwrap_or(UNLIMITED)),
            used: AtomicU64::new(0),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(crate) fn limit(&self) -> Option<u64> {
        Some(self.limit.load(Ordering::Relaxed)).filter(|limit| *limit != UNLIMITED)
    }

    /// 调整页数上限（tmpfs `remount` 的 `size=`）。
    ///
    /// # Errors
    ///
    /// 新上限小于已占用页数返回 `InvalidOperation`（Linux：`Cannot retroactively limit size`）。
    pub(crate) fn set_limit(&self, limit: Option<u64>) -> Result<(), FileSystemError> {
        let limit = limit.unwrap_or(UNLIMITED);
        // 先把上限抬到新值再校验占用会让并发预留越界；因此校验与写入必须是同一个 CAS 序列：
        // 当前 `used` 不超过新上限时才发布，发布后并发的 `try_reserve` 会按新上限判定。
        loop {
            let current = self.limit.load(Ordering::Relaxed);
            if self.used.load(Ordering::Acquire) > limit {
                return Err(FileSystemError::InvalidOperation);
            }
            if self
                .limit
                .compare_exchange(current, limit, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                // 发布之后可能有预留在旧上限下完成；若它们使 used 越过新上限，回滚发布。
                if self.used.load(Ordering::Acquire) > limit {
                    self.limit.store(current, Ordering::Release);
                    return Err(FileSystemError::InvalidOperation);
                }
                return Ok(());
            }
        }
    }

    pub(crate) fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    fn try_reserve(&self) -> bool {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                (used < self.limit.load(Ordering::Acquire)).then(|| used + 1)
            })
            .is_ok()
    }

    fn release(&self) {
        let previous = self.used.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(previous, 0, "page budget released more pages than reserved");
    }
}

/// 一个存储页：物理页加它占用的配额。
struct ResidentPage {
    frame: SharedFrame,
    budget: Arc<PageBudget>,
    /// 所属文件的修改时间；writable 映射建立时更新（Linux `page_mkwrite` 的 `file_update_time`）。
    modified_ns: Arc<AtomicU64>,
}

impl Drop for ResidentPage {
    fn drop(&mut self) {
        self.budget.release();
    }
}

impl core::fmt::Debug for ResidentPage {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("ResidentPage")
            .finish_non_exhaustive()
    }
}

impl PageBytes for ResidentPage {
    fn read(&self, offset: usize, output: &mut [u8]) {
        self.frame.read(offset, output);
    }

    fn write(&self, offset: usize, input: &[u8]) {
        self.frame.write(offset, input);
    }

    fn zero_from(&self, offset: usize) {
        self.frame.zero_from(offset);
    }
}

impl SharedPage for ResidentPage {
    fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    // 存储页永远不会被写回或回收，没有 dirty 状态，writer 计数没有消费者。但 writable 映射的首次
    // 建立意味着文件内容即将被 store 修改，必须在此刻推进 mtime：之后的 store 不再经过内核。
    fn acquire_writer(&self) {
        touch(&self.modified_ns);
    }

    fn release_writer(&self) {}
}

/// 页分配失败的原因。
#[derive(Debug, Clone, Copy)]
enum PageShortage {
    /// 超过 filesystem 的页配额（`ENOSPC`）。
    Quota,
    /// 物理内存或 Arc 分配失败（`ENOMEM`）。
    Memory,
}

/// 文件长度、内容页与 seal 的单一线性化 owner。
struct State {
    pages: SparsePages<ResidentPage>,
    seals: Seals,
}

/// 内存型 regular file。
pub(crate) struct MemoryFile {
    id: SharedFileId,
    budget: Arc<PageBudget>,
    // OWNER: 序列化整次 regular write、append、truncate 与 fallocate；caller 在持有期间会做用户态
    // 拷贝并进入 AddressSpace lock，所以它必须在 `state` 之外，page fault 只取内层 `state`。
    write_sequence: TaskMutex<()>,
    // OWNER: 文件长度、页映射与 seal 的权威状态。TaskMutex：页分配可能进入 direct reclaim 并
    // 等待 I/O，持 spin lock 睡眠会让同 CPU 的另一个 owner 永久自旋。
    state: TaskMutex<State>,
    // 文件长度的无锁只读投影，只在持有 `state` 时由 mutation 更新；权威仍是 `state`。
    // `Inode::size` 不能失败也不能睡眠，缺失该投影只能在锁获取失败时回答错误的长度。
    published_length: AtomicU64,
    // 最近一次内容修改的 realtime（ns）。写入与 mmap 直接作用于这里而不经 inode，所以修改时间只能由
    // 存储自己记录；缺失时 tmpfs/memfd 的 `st_mtime` 永远停在创建时刻，`make` 之类按 mtime 判断的
    // 工具会漏掉改动。
    modified_ns: Arc<AtomicU64>,
}

/// 持有整次 regular write 的序列化权；Drop 释放。
pub(crate) struct MemoryWrite<'a> {
    file: &'a MemoryFile,
    _sequence: TaskMutexGuard<'a, ()>,
}

fn shortage_error(stop: Stop<PageShortage>) -> FileSystemError {
    match stop {
        Stop::Allocator(PageShortage::Quota) => FileSystemError::NoSpace,
        Stop::OutOfMemory | Stop::Allocator(PageShortage::Memory) => FileSystemError::OutOfMemory,
    }
}

fn seal_error(error: SealError) -> FileSystemError {
    match error {
        SealError::InvalidOperation => FileSystemError::InvalidOperation,
        SealError::PermissionDenied => FileSystemError::PermissionDenied,
    }
}

impl MemoryFile {
    /// 创建空文件。
    ///
    /// # Parameters
    ///
    /// - `id`: 文件所属 filesystem 实例与 inode，用于 mmap 失效广播。
    /// - `budget`: 页配额。
    /// - `allow_sealing`: memfd 的 `MFD_ALLOW_SEALING`；普通 tmpfs 文件为 false。
    pub(crate) fn new(
        id: SharedFileId,
        budget: Arc<PageBudget>,
        allow_sealing: bool,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            id,
            budget,
            write_sequence: TaskMutex::new(()),
            state: TaskMutex::new(State {
                pages: SparsePages::new(),
                seals: Seals::new(allow_sealing),
            }),
            published_length: AtomicU64::new(0),
            modified_ns: Arc::try_new(AtomicU64::new(0))
                .map_err(|_| FileSystemError::OutOfMemory)?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn state(&self) -> Result<TaskMutexGuard<'_, State>, FileSystemError> {
        self.state.lock().map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 分配一个已预留配额的零页。
    fn allocate_page(&self) -> Result<Arc<ResidentPage>, PageShortage> {
        if !self.budget.try_reserve() {
            return Err(PageShortage::Quota);
        }
        // `page` 一旦构造，任何后续失败都由它的 Drop 归还配额。
        let page = SharedFrame::allocate()
            .map(|frame| ResidentPage {
                frame,
                budget: self.budget.clone(),
                modified_ns: self.modified_ns.clone(),
            })
            .map_err(|_| {
                self.budget.release();
                PageShortage::Memory
            })?;
        Arc::try_new(page).map_err(|_| PageShortage::Memory)
    }

    /// 该文件在 mmap 失效广播与 page-cache 之外的唯一身份。
    pub(crate) fn id(&self) -> SharedFileId {
        self.id
    }

    /// 当前文件长度。
    pub(crate) fn size(&self) -> u64 {
        self.published_length.load(Ordering::Acquire)
    }

    /// 最近一次内容修改的 realtime 秒数；从未修改为 0。
    pub(crate) fn modified_seconds(&self) -> u64 {
        self.modified_ns.load(Ordering::Relaxed) / 1_000_000_000
    }

    fn touch(&self) {
        touch(&self.modified_ns);
    }

    /// 在持有 `state` 时发布新的文件长度。
    fn publish_length(&self, state: &State) {
        self.published_length
            .store(state.pages.len(), Ordering::Release);
    }

    /// 已分配的页数（不含稀疏文件的洞）。
    pub(crate) fn resident_pages(&self) -> Result<usize, FileSystemError> {
        Ok(self.state()?.pages.resident_pages())
    }

    pub(crate) fn seals(&self) -> Result<u32, FileSystemError> {
        Ok(self.state()?.seals.bits())
    }

    /// 追加 seal；见 [`Seals::add`]。
    pub(crate) fn add_seals(&self, seals: u32) -> Result<u32, FileSystemError> {
        self.state()?.seals.add(seals).map_err(seal_error)
    }

    /// 读取；洞读为零，EOF 返回零字节。
    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        Ok(self.state()?.pages.read(offset, output))
    }

    /// 开始一次不可被其他 mutation 穿插的 regular write。
    pub(crate) fn begin_write(&self) -> Result<MemoryWrite<'_>, FileSystemError> {
        Ok(MemoryWrite {
            file: self,
            _sequence: self
                .write_sequence
                .lock()
                .map_err(|_| FileSystemError::OutOfMemory)?,
        })
    }

    fn write_locked(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        if input.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let mut state = self.state()?;
        let length = state.pages.len();
        state.seals.check_write(length, end).map_err(seal_error)?;
        let (written, stop) = state.pages.write(offset, input, || self.allocate_page());
        self.publish_length(&state);
        if written != 0 {
            self.touch();
        }
        match (written, stop) {
            (0, Some(stop)) => Err(shortage_error(stop)),
            // 已写入的部分按 POSIX 短写返回；下一次写入会再次遇到同一个停止原因。
            (written, _) => Ok(written),
        }
    }

    /// 截断（或增长为稀疏文件）并撤销所有 mmap 对被截掉页的映射。
    ///
    /// # Errors
    ///
    /// seal 禁止、分配等待元数据失败返回对应错误。
    pub(crate) fn truncate(&self, size: u64) -> Result<(), FileSystemError> {
        // 失效广播发生在不可回滚的页释放之后；预先分配 waiter，使该阶段不会失败。
        let mut invalidation =
            TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
        let _sequence = self
            .write_sequence
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        {
            let mut state = self.state()?;
            let length = state.pages.len();
            state
                .seals
                .check_truncate(length, size)
                .map_err(seal_error)?;
            state.pages.truncate(size);
            self.publish_length(&state);
        }
        self.touch();
        invalidate_shared_file(self.id, size, &mut invalidation);
        Ok(())
    }

    /// 预分配 `[offset, offset + length)`，并把文件长度提升到范围末端。
    pub(crate) fn allocate(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        let _sequence = self
            .write_sequence
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let end = offset
            .checked_add(length)
            .ok_or(FileSystemError::InvalidOperation)?;
        let mut state = self.state()?;
        let current = state.pages.len();
        state.seals.check_write(current, end).map_err(seal_error)?;
        let result = state
            .pages
            .allocate_range(offset, length, || self.allocate_page())
            .map_err(shortage_error);
        self.publish_length(&state);
        if result.is_ok() {
            self.touch();
        }
        result
    }

    /// 建立 mmap 视图；映射持有存储本身，因此可以比 inode 存活更久。
    pub(crate) fn mapping(self: &Arc<Self>) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
        Arc::try_new(MemoryMapping(self.clone()))
            .map(|mapping| mapping as Arc<dyn SharedFileMapping>)
            .map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl MemoryWrite<'_> {
    /// 在 `offset` 写入；返回实际写入的字节数（页配额或内存耗尽时为短写）。
    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        self.file.write_locked(offset, input)
    }

    /// 原子追加，受 `size_limit`（RLIMIT_FSIZE）约束。
    ///
    /// # Returns
    ///
    /// 追加的起始 offset 与字节数；已到上限时字节数为零，由 syscall 生成 SIGXFSZ/EFBIG。
    pub(crate) fn append(
        &self,
        input: &[u8],
        size_limit: u64,
    ) -> Result<(u64, usize), FileSystemError> {
        // write、append、truncate 与 fallocate 都持有 `write_sequence`，本 facade 已独占它，
        // 所以读到的长度在这次写入完成前不会变化。
        let offset = self.file.size();
        let allowed = usize::try_from(size_limit.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(input.len());
        if allowed == 0 {
            return Ok((offset, 0));
        }
        self.file
            .write_locked(offset, &input[..allowed])
            .map(|written| (offset, written))
    }
}

/// 一个 [`MemoryFile`] 的 mmap 视图。
struct MemoryMapping(Arc<MemoryFile>);

impl core::fmt::Debug for MemoryMapping {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MemoryMapping")
            .finish_non_exhaustive()
    }
}

impl SharedFileMapping for MemoryMapping {
    fn id(&self) -> SharedFileId {
        self.0.id
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn page(&self, index: u64) -> Result<Arc<dyn SharedPage>, SharedFileError> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| SharedFileError::OutOfMemory)?;
        match state.pages.page(index, || self.0.allocate_page()) {
            None => Err(SharedFileError::BeyondEof),
            Some(Ok(page)) => Ok(page as Arc<dyn SharedPage>),
            // 配额耗尽时 mmap 访问以 SIGBUS 报告，与 Linux tmpfs 超出 `size=` 的行为一致。
            Some(Err(Stop::Allocator(PageShortage::Quota))) => Err(SharedFileError::Io),
            Some(Err(_)) => Err(SharedFileError::OutOfMemory),
        }
    }

    // 内存型文件没有 backing storage，msync 无事可做。
    fn sync_range(&self, _offset: u64, _length: u64) -> Result<(), SharedFileError> {
        Ok(())
    }
}
