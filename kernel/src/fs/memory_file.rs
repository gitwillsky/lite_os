//! 内存型 regular file 的唯一存储：页即内容，单份，生命周期跟随持有它的 inode。
//!
//! tmpfs 与 memfd 共用；canonical 内容页可迁移到 swap，但没有第二份文件镜像，不经磁盘 page cache：
//! 页缓存的 registry 会让已删除文件的 inode（连同内容）一直存活到下一次 `sync`，并让热数据
//! 占两份内存。读写、mmap 与 truncate 都直接作用于这些页；页只在文件被删除且没有 open/mmap
//! 引用时随 `Arc` 释放，并立即归还配额。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{FileSystemError, StorageBudget};
use crate::memory::{
    MemoryReclaimer, PAGE_SIZE, ReclaimRequest, ReclaimResult, SharedFileError, SharedFileId,
    SharedFileMapping, SharedPage, SharedPagePin, SwappablePage, invalidate_shared_file,
    register_memory_reclaimer,
};
use crate::sync::{TaskMutex, TaskMutexGuard, TaskMutexWaitPreparation};

mod seals;
mod sparse;
use seals::{SealError, Seals};
use sparse::{PageBytes, SparsePages, Stop};

const _: () = assert!(sparse::PAGE_SIZE == PAGE_SIZE);

/// 一个存储页：唯一可换出内容 owner 加它持续占用的配额。
struct ResidentPage {
    page: Arc<SwappablePage>,
    budget: Arc<StorageBudget>,
    /// 所属文件的修改时间；writable 映射建立时更新（Linux `page_mkwrite` 的 `file_update_time`）。
    times: Arc<super::inode_times::InodeTimes>,
}

impl Drop for ResidentPage {
    fn drop(&mut self) {
        self.budget.release_page();
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
    type Error = SharedFileError;
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), Self::Error> {
        self.page.pin()?.read(offset, output);
        Ok(())
    }

    fn write(&self, offset: usize, input: &[u8]) -> Result<(), Self::Error> {
        self.page.pin()?.write(offset, input);
        Ok(())
    }

    fn zero_from(&self, offset: usize) -> Result<(), Self::Error> {
        self.page.pin()?.zero_from(offset);
        Ok(())
    }
}

impl SharedPage for ResidentPage {
    fn pin(&self) -> Result<SharedPagePin<'_>, SharedFileError> {
        self.page.pin().map(SharedPagePin::Swappable)
    }

    fn swappable(&self) -> Option<&Arc<SwappablePage>> {
        Some(&self.page)
    }

    // 存储页没有独立 writeback dirty 状态，writer 计数没有消费者。但 writable 映射的首次
    // 建立意味着文件内容即将被 store 修改，必须在此刻推进 mtime：之后的 store 不再经过内核。
    fn acquire_writer(&self) {
        self.times.touch();
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
    /// 只为粗粒度 reclaimer 提供轮转索引，不保存另一份页内容或页集合。
    reclaim_cursor: u64,
}

/// 内存型 regular file。
pub(crate) struct MemoryFile {
    id: SharedFileId,
    budget: Arc<StorageBudget>,
    // OWNER: 序列化整次 regular write、append、truncate 与 fallocate；caller 在持有期间会做用户态
    // 拷贝并进入 AddressSpace lock，所以它必须在 `state` 之外，page fault 只取内层 `state`。
    write_sequence: TaskMutex<()>,
    // OWNER: 文件长度、页映射与 seal 的权威状态。TaskMutex：页分配可能进入 direct reclaim 并
    // 等待 I/O，持 spin lock 睡眠会让同 CPU 的另一个 owner 永久自旋。
    state: TaskMutex<State>,
    // 文件长度的无锁只读投影，只在持有 `state` 时由 mutation 更新；权威仍是 `state`。
    // `Inode::size` 不能失败也不能睡眠，缺失该投影只能在锁获取失败时回答错误的长度。
    published_length: AtomicU64,
    // OWNER: inode 与所有 writable resident-page 共用同一 atime/mtime/ctime tuple；
    // mmap touch 不调用 inode metadata，也不以 mtime 的最大值伪造 ctime。
    times: Arc<super::inode_times::InodeTimes>,
}

/// 持有整次 regular write 的序列化权；Drop 释放。
pub(crate) struct MemoryWrite<'a> {
    file: &'a MemoryFile,
    _sequence: TaskMutexGuard<'a, ()>,
}

fn shortage_error(stop: Stop<PageShortage, SharedFileError>) -> FileSystemError {
    match stop {
        Stop::Access(error) => page_access_error(error),
        Stop::Allocator(PageShortage::Quota) => FileSystemError::NoSpace,
        Stop::OutOfMemory | Stop::Allocator(PageShortage::Memory) => FileSystemError::OutOfMemory,
    }
}

fn page_access_error(error: SharedFileError) -> FileSystemError {
    match error {
        SharedFileError::OutOfMemory => FileSystemError::OutOfMemory,
        SharedFileError::Io | SharedFileError::BeyondEof => FileSystemError::IoError,
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
    pub(super) fn new(
        id: SharedFileId,
        budget: Arc<StorageBudget>,
        allow_sealing: bool,
    ) -> Result<Arc<Self>, FileSystemError> {
        let file = Arc::try_new(Self {
            id,
            budget,
            write_sequence: TaskMutex::new(()),
            state: TaskMutex::new(State {
                pages: SparsePages::new(),
                seals: Seals::new(allow_sealing),
                reclaim_cursor: 0,
            }),
            published_length: AtomicU64::new(0),
            times: super::inode_times::InodeTimes::new()?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        register_memory_reclaimer(file.clone()).map_err(page_access_error)?;
        Ok(file)
    }

    fn state(&self) -> Result<TaskMutexGuard<'_, State>, FileSystemError> {
        self.state.lock().map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 分配一个已预留配额的零页。
    fn allocate_page(&self) -> Result<Arc<ResidentPage>, PageShortage> {
        if !self.budget.reserve_page() {
            return Err(PageShortage::Quota);
        }
        // `page` 一旦构造，任何后续失败都由它的 Drop 归还配额。
        let page = SwappablePage::allocate_shared()
            .map(|page| ResidentPage {
                page,
                budget: self.budget.clone(),
                times: self.times.clone(),
            })
            .map_err(|_| {
                self.budget.release_page();
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

    /// inode metadata 与 mmap writer 共用的时间 owner；不复制 scalar 时间。
    pub(super) fn times(&self) -> Arc<super::inode_times::InodeTimes> {
        self.times.clone()
    }

    fn touch(&self) {
        self.times.touch();
    }

    /// 在持有 `state` 时发布新的文件长度。
    fn publish_length(&self, state: &State) {
        self.published_length
            .store(state.pages.len(), Ordering::Release);
    }

    /// 已分配的页数（不含稀疏文件的洞）。
    pub(crate) fn allocated_pages(&self) -> Result<usize, FileSystemError> {
        Ok(self.state()?.pages.allocated_pages())
    }

    pub(crate) fn seals(&self) -> Result<u32, FileSystemError> {
        Ok(self.state()?.seals.bits())
    }

    /// 追加 seal；见 [`Seals::add`]。
    pub(crate) fn add_seals(&self, seals: u32) -> Result<u32, FileSystemError> {
        // seals 与写入/resize 共用 sequence；truncate 的提前检查直到提交都有效。
        let _sequence = self
            .write_sequence
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.state()?.seals.add(seals).map_err(seal_error)
    }

    /// 读取；洞读为零，EOF 返回零字节。
    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let (read, error) = self.state()?.pages.read(offset, output);
        match (read, error) {
            (0, Some(error)) => Err(page_access_error(error)),
            (read, _) => Ok(read),
        }
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

    /// 同 write sequence 先判 seal，再准备 metadata guard；成功 resize 后无失败提交属性。
    ///
    /// `prepare` 可失败但不改变属性；`commit` 不分配、不失败。广播发生在 pages/metadata guards 外。
    pub(super) fn truncate_with_metadata<G>(
        &self,
        size: u64,
        prepare: impl FnOnce() -> Result<G, FileSystemError>,
        commit: impl FnOnce(&mut G),
    ) -> Result<(), FileSystemError> {
        // 失效广播发生在不可回滚的页释放之后；预先分配 waiter，使该阶段不会失败。
        let mut invalidation =
            TaskMutexWaitPreparation::prepare().map_err(|_| FileSystemError::OutOfMemory)?;
        let _sequence = self
            .write_sequence
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        {
            let state = self.state()?;
            state
                .seals
                .check_truncate(state.pages.len(), size)
                .map_err(seal_error)?;
        }
        let mut metadata = prepare()?;
        {
            let mut state = self.state()?;
            state.pages.truncate(size).map_err(page_access_error)?;
            self.publish_length(&state);
        }
        commit(&mut metadata);
        self.touch();
        drop(metadata);
        invalidate_shared_file(self.id, size, &mut invalidation);
        Ok(())
    }

    /// 成功 allocation 的同一 write sequence 内更新 inode metadata，不跨 metadata guard 访问页。
    pub(super) fn allocate_with_metadata(
        &self,
        offset: u64,
        length: u64,
        modified: impl FnOnce(),
    ) -> Result<(), FileSystemError> {
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
        drop(state);
        if result.is_ok() {
            modified();
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
    pub(super) fn belongs_to(&self, file: &MemoryFile) -> bool {
        core::ptr::eq(self.file, file)
    }
    pub(super) fn size(&self) -> u64 {
        self.file.size()
    }

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

impl MemoryReclaimer for MemoryFile {
    fn reclaim_pages(&self, request: ReclaimRequest) -> ReclaimResult {
        if !request.allows_swap_io() {
            return ReclaimResult::default();
        }
        let mut scanned = 0;
        let mut reclaimed = 0;
        let mut wrapped = false;
        let mut initial_cursor = None;
        while scanned < request.scan_pages() && reclaimed < request.target_pages() {
            let candidate = {
                let Some(mut state) = self.state.try_lock() else {
                    break;
                };
                let mut next = state.pages.next_page_from(state.reclaim_cursor);
                let initial = *initial_cursor.get_or_insert(state.reclaim_cursor);
                if next.is_none() && !wrapped {
                    wrapped = true;
                    next = state.pages.next_page_from(0);
                }
                let Some((index, page)) = next else {
                    break;
                };
                if wrapped && index >= initial {
                    break;
                }
                state.reclaim_cursor = index
                    .checked_add(1)
                    .expect("memory-file page index overflow");
                page.page.clone()
            };
            scanned += 1;
            // PageOp -> try-MM only; never enter this IO with file State or write_sequence held.
            reclaimed += usize::from(candidate.swap_out());
        }
        ReclaimResult::new(reclaimed, scanned)
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
            Some(Err(Stop::Access(error))) => Err(error),
            Some(Err(Stop::OutOfMemory | Stop::Allocator(PageShortage::Memory))) => {
                Err(SharedFileError::OutOfMemory)
            }
        }
    }

    // 内存型文件没有 backing storage，msync 无事可做。
    fn sync_range(&self, _offset: u64, _length: u64) -> Result<(), SharedFileError> {
        Ok(())
    }
}
