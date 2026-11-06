//! 内存型文件（tmpfs、memfd）：内容就是一个可调整大小的 VMO，没有后端存储。
//!
//! 1. 文件长度单独发布；VMO 大小是长度按页取整，页在首次写入时由内核提交。
//! 2. 配额属于实际文件页：稀疏增长不预留，write/fallocate/pager 补页先准入，截断与最后释放归还。
//! 3. 共享映射直接映射该 VMO，因此 read/write 与 mmap 看到同一页；截断缩小 VMO 时内核撤销越界映射。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use fallible_tree::{FallibleMap, VacantEntry};
use native_runtime::{Pager, Vmo};

use native_runtime::native_abi::vmo;

use super::{FileSystemError, StorageBudget};
use crate::memory::{MemoryObject, PAGE_SIZE, SharedFileError, SharedFileId, SharedFileMapping};
use crate::sync::{TaskMutex, TaskMutexGuard};

mod seals;
use seals::{SealError, Seals};

struct State {
    seals: Seals,
    // OWNER: 稀疏文件页的配额 lease；只有 supply 成功才发表，VMO 只拥有物理页机制。
    // read 的洞判定也借此索引，不为读取洞创建页；截断先退休 VMO 页再释放相应 lease。
    pages: FallibleMap<u64, PageCharge>,
}

struct PageCharge(Arc<StorageBudget>);

// OWNER: 全部 tmpfs/memfd 已计费页数（`/proc/meminfo` 的 Shmem），随 PageCharge 构造与析构增减；
// 只作统计、不参与配额判定。缺失时只能逐个遍历全部文件，或把未映射的 tmpfs 页漏出 Shmem。
static CHARGED_PAGES: AtomicUsize = AtomicUsize::new(0);

impl PageCharge {
    fn new(budget: Arc<StorageBudget>) -> Self {
        CHARGED_PAGES.fetch_add(1, Ordering::Relaxed);
        Self(budget)
    }
}

impl Drop for PageCharge {
    fn drop(&mut self) {
        CHARGED_PAGES.fetch_sub(1, Ordering::Relaxed);
        self.0.release_page();
    }
}

/// tmpfs/memfd 内容占用的页数。
pub(crate) fn charged_pages() -> usize {
    CHARGED_PAGES.load(Ordering::Relaxed)
}

/// tmpfs/memfd 文件内容。
pub(crate) struct MemoryFile {
    id: SharedFileId,
    budget: Arc<StorageBudget>,
    object: Arc<MemoryObject>,
    key: u64,
    // OWNER: 写入、截断与封印修改的唯一串行化点；长度发布与 VMO 大小调整在其中一致完成。
    write_sequence: TaskMutex<()>,
    state: TaskMutex<State>,
    // OWNER: 存活的可写共享映射数；F_SEAL_WRITE 在其非零时失败（Linux mapping_writably_mapped）。
    writable_mappings: AtomicUsize,
    length: AtomicU64,
    times: Arc<super::inode_times::InodeTimes>,
}

/// 一次写事务（持有写串行化锁）。
pub(crate) struct MemoryWrite<'a> {
    file: &'a MemoryFile,
    _sequence: TaskMutexGuard<'a, ()>,
}

fn seal_error(error: SealError) -> FileSystemError {
    match error {
        SealError::InvalidOperation => FileSystemError::InvalidOperation,
        SealError::PermissionDenied => FileSystemError::PermissionDenied,
    }
}

fn object_error(error: crate::memory::MemoryError) -> FileSystemError {
    if error.is_out_of_memory() {
        FileSystemError::OutOfMemory
    } else {
        FileSystemError::IoError
    }
}

fn pages_for(length: u64) -> u64 {
    length.div_ceil(PAGE_SIZE as u64)
}

impl MemoryFile {
    pub(super) fn new(
        id: SharedFileId,
        budget: Arc<StorageBudget>,
        allow_sealing: bool,
    ) -> Result<Arc<Self>, FileSystemError> {
        let (object, key) = super::pager::create_object(0)?;
        let file = Arc::try_new(Self {
            id,
            budget,
            object,
            key,
            write_sequence: TaskMutex::new(()),
            state: TaskMutex::new(State {
                seals: Seals::new(allow_sealing),
                pages: FallibleMap::new(),
            }),
            writable_mappings: AtomicUsize::new(0),
            length: AtomicU64::new(0),
            times: super::inode_times::InodeTimes::new()?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        super::pager::register(key, &file)?;
        Ok(file)
    }

    fn state(&self) -> Result<TaskMutexGuard<'_, State>, FileSystemError> {
        self.state.lock().map_err(|_| FileSystemError::OutOfMemory)
    }

    fn sequence(&self) -> Result<TaskMutexGuard<'_, ()>, FileSystemError> {
        self.write_sequence
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)
    }

    pub(crate) fn id(&self) -> SharedFileId {
        self.id
    }

    pub(crate) fn size(&self) -> u64 {
        self.length.load(Ordering::Acquire)
    }

    pub(super) fn times(&self) -> Arc<super::inode_times::InodeTimes> {
        self.times.clone()
    }

    /// 内核已为内容提交的页数（`st_blocks`）。
    pub(crate) fn allocated_pages(&self) -> Result<usize, FileSystemError> {
        Ok(self.object.committed_bytes().map_err(object_error)? as usize / PAGE_SIZE)
    }

    pub(crate) fn seals(&self) -> Result<u32, FileSystemError> {
        Ok(self.state()?.seals.bits())
    }

    pub(crate) fn add_seals(&self, seals: u32) -> Result<u32, FileSystemError> {
        let _sequence = self.sequence()?;
        let mut state = self.state()?;
        let mut candidate = state.seals;
        candidate.add(seals).map_err(seal_error)?;
        if seals & seals::F_SEAL_WRITE != 0 && self.writable_mappings.load(Ordering::Acquire) != 0 {
            return Err(FileSystemError::Busy);
        }
        state.seals = candidate;
        Ok(candidate.bits())
    }

    pub(crate) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let state = self.state()?;
        let length = self.size();
        if offset >= length {
            return Ok(0);
        }
        let count = output.len().min((length - offset) as usize);
        let mut cursor = offset;
        let end = offset + count as u64;
        let mut pages = state
            .pages
            .iter_from(&(offset / PAGE_SIZE as u64))
            .peekable();
        while cursor < end {
            let start = (cursor - offset) as usize;
            let Some(&(&index, _)) = pages.peek() else {
                output[start..count].fill(0);
                break;
            };
            let page_start = index * PAGE_SIZE as u64;
            if cursor < page_start {
                let hole_end = page_start.min(end);
                output[start..(hole_end - offset) as usize].fill(0);
                cursor = hole_end;
                continue;
            }
            pages.next();
            let mut run_end = page_start + PAGE_SIZE as u64;
            while run_end < end
                && pages
                    .peek()
                    .is_some_and(|&(&next, _)| next == run_end / PAGE_SIZE as u64)
            {
                pages.next();
                run_end += PAGE_SIZE as u64;
            }
            let run_end = run_end.min(end);
            self.object
                .vmo()
                .read(&mut output[start..(run_end - offset) as usize], cursor)
                .map_err(|_| FileSystemError::IoError)?;
            cursor = run_end;
        }
        Ok(count)
    }

    pub(crate) fn begin_write(&self) -> Result<MemoryWrite<'_>, FileSystemError> {
        Ok(MemoryWrite {
            file: self,
            _sequence: self.sequence()?,
        })
    }

    /// 调整逻辑 EOF 和按页取整的 VMO，不因稀疏增长消耗存储配额。
    /// 调用者持有写串行化与 state；缩小先清零实际末页，再退休越界物理页与 quota lease。
    fn resize_locked(&self, state: &mut State, length: u64) -> Result<(), FileSystemError> {
        let vmo_size = length
            .checked_next_multiple_of(PAGE_SIZE as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        if length < self.size()
            && length < vmo_size
            && state.pages.contains_key(&(length / PAGE_SIZE as u64))
        {
            self.object
                .vmo()
                .op_range(vmo::OP_ZERO, length, vmo_size - length)
                .map_err(|_| FileSystemError::IoError)?;
        }
        let old_size = self.object.size().map_err(object_error)?;
        if old_size != vmo_size {
            self.object.set_size(vmo_size).map_err(object_error)?;
        }
        if vmo_size < old_size {
            state.pages.retain(|index, _| *index < pages_for(length));
        }
        self.length.store(length, Ordering::Release);
        Ok(())
    }

    fn prepare_page(&self, index: u64) -> Result<VacantEntry<u64, PageCharge>, FileSystemError> {
        if !self.budget.reserve_page() {
            return Err(FileSystemError::NoSpace);
        }
        FallibleMap::try_prepare(index, PageCharge::new(self.budget.clone()))
            .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn supply_zero(
        &self,
        pager: &Pager,
        index: u64,
        aux: &mut Option<Vmo>,
    ) -> Result<(), FileSystemError> {
        if aux.is_none() {
            *aux =
                Some(Vmo::create(PAGE_SIZE as u64, 0).map_err(|_| FileSystemError::OutOfMemory)?);
        }
        let aux = aux.as_ref().expect("auxiliary VMO created above");
        aux.write(&[0; PAGE_SIZE], 0)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        pager
            .supply_pages(
                self.object.vmo(),
                index * PAGE_SIZE as u64,
                PAGE_SIZE as u64,
                aux,
                0,
            )
            .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn commit_page(
        &self,
        state: &mut State,
        pager: &Pager,
        index: u64,
        aux: &mut Option<Vmo>,
    ) -> Result<(), FileSystemError> {
        if state.pages.contains_key(&index) {
            return Ok(());
        }
        let entry = self.prepare_page(index)?;
        self.supply_zero(pager, index, aux)?;
        state.pages.commit_vacant(entry);
        Ok(())
    }

    fn write_locked(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        if input.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(FileSystemError::InvalidOperation)?;
        let mut state = self.state()?;
        let length = self.size();
        state.seals.check_write(length, end).map_err(seal_error)?;
        if end > length {
            let size = end
                .checked_next_multiple_of(PAGE_SIZE as u64)
                .ok_or(FileSystemError::InvalidOperation)?;
            self.object.set_size(size).map_err(object_error)?;
        }
        let mut done = 0;
        let mut aux = None;
        let mut failure = None;
        while done < input.len() {
            let cursor = offset + done as u64;
            let chunk = (PAGE_SIZE - (cursor % PAGE_SIZE as u64) as usize).min(input.len() - done);
            let result = self.commit_page(
                &mut state,
                super::pager::handle(),
                cursor / PAGE_SIZE as u64,
                &mut aux,
            );
            if let Err(error) = result {
                failure = Some(error);
                break;
            }
            done += chunk;
        }
        // 稳态连续写仍只有一次 native copy；配额准入失败时只复制已准入的前缀。
        if done > 0
            && let Err(status) = self.object.vmo().write(&input[..done], offset)
        {
            done = 0;
            failure = Some(if status == native_runtime::native_abi::ERR_NO_MEMORY {
                FileSystemError::OutOfMemory
            } else {
                FileSystemError::IoError
            });
        }
        // quota 失败可返回已写前缀；不能把尚未写入的请求尾部发表成 EOF。
        let final_length = if done == 0 {
            length
        } else {
            length.max(offset + done as u64)
        };
        if end > length {
            self.resize_locked(&mut state, final_length)?;
        }
        drop(state);
        if done > 0 {
            self.times.touch();
            Ok(done)
        } else {
            Err(failure.expect("nonempty write without progress must fail"))
        }
    }

    pub(super) fn truncate_with_metadata<G>(
        &self,
        size: u64,
        prepare: impl FnOnce() -> Result<G, FileSystemError>,
        commit: impl FnOnce(&mut G),
    ) -> Result<(), FileSystemError> {
        let _sequence = self.sequence()?;
        let mut state = self.state()?;
        state
            .seals
            .check_truncate(self.size(), size)
            .map_err(seal_error)?;
        let mut metadata = prepare()?;
        self.resize_locked(&mut state, size)?;
        drop(state);
        commit(&mut metadata);
        self.times.touch();
        Ok(())
    }

    pub(super) fn allocate_with_metadata(
        &self,
        offset: u64,
        length: u64,
        modified: impl FnOnce(),
    ) -> Result<(), FileSystemError> {
        let _sequence = self.sequence()?;
        let end = offset
            .checked_add(length)
            .ok_or(FileSystemError::InvalidOperation)?;
        let mut state = self.state()?;
        let current = self.size();
        state.seals.check_growth(current, end).map_err(seal_error)?;
        let mut pending = FallibleMap::new();
        // 先准入全部缺页；限额/metadata OOM 不改变文件或既有页。
        for index in offset / PAGE_SIZE as u64..pages_for(end) {
            if !state.pages.contains_key(&index) {
                pending.commit_vacant(self.prepare_page(index)?);
            }
        }
        let old_size = self.object.size().map_err(object_error)?;
        let size = end
            .checked_next_multiple_of(PAGE_SIZE as u64)
            .ok_or(FileSystemError::InvalidOperation)?
            .max(old_size);
        if size != old_size {
            self.object.set_size(size).map_err(object_error)?;
        }
        let mut aux = None;
        for (&index, _) in pending.iter() {
            if let Err(error) = self.supply_zero(super::pager::handle(), index, &mut aux) {
                // 所有 pending key 原来都是洞；保留 lease 到撤销/fence 完成，不碰既有内容。
                for (&index, _) in pending.iter() {
                    self.object
                        .vmo()
                        .op_range(vmo::OP_DECOMMIT, index * PAGE_SIZE as u64, PAGE_SIZE as u64)
                        .expect("failed file allocation rollback must retire supplied pages");
                }
                if size != old_size {
                    self.object.set_size(old_size).map_err(object_error)?;
                }
                return Err(error);
            }
        }
        while let Some((&index, _)) = pending.first_key_value() {
            state.pages.commit_vacant(
                pending
                    .take_entry(&index)
                    .expect("prepared file page disappeared"),
            );
        }
        self.length.store(current.max(end), Ordering::Release);
        drop(state);
        modified();
        self.times.touch();
        Ok(())
    }

    pub(crate) fn mapping(
        self: &Arc<Self>,
        shared: bool,
        may_write: bool,
        write: bool,
    ) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
        let mut mode = MappingWrite::Private;
        if shared {
            let allowed = !self.state()?.seals.blocks_new_writes();
            if write && !allowed {
                return Err(FileSystemError::PermissionDenied);
            }
            mode = if may_write && allowed {
                self.writable_mappings
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                        count.checked_add(1)
                    })
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                MappingWrite::SharedWritable
            } else {
                MappingWrite::SharedReadOnly
            };
        }
        let mapping = Arc::try_new(MemoryMapping {
            file: self.clone(),
            write: mode,
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(mapping as Arc<dyn SharedFileMapping>)
    }
}

impl Drop for MemoryFile {
    fn drop(&mut self) {
        super::pager::retire(self.key, &self.object);
        // object 先于 state 析构：物理 VMO 退休后才由 PageCharge Drop 归还存储名额。
    }
}

impl super::pager::PageSource for MemoryFile {
    fn page_request(&self, pager: &Pager, offset: u64, length: u64, aux: &mut Option<Vmo>) {
        let result = self.state().and_then(|mut state| {
            for index in offset / PAGE_SIZE as u64..pages_for(offset + length) {
                if index >= pages_for(self.size()) {
                    return Err(FileSystemError::InvalidOperation);
                }
                // 已计费的页再次缺页：内核在内存紧张时淘汰了它。内存文件从不回写，能被淘汰的只有补入后
                // 从未写过的零页，因此重新补零页；缺失时请求永远得不到 supply，访问者永久睡眠。
                if state.pages.contains_key(&index) {
                    self.supply_zero(pager, index, aux)?;
                } else {
                    self.commit_page(&mut state, pager, index, aux)?;
                }
            }
            Ok(())
        });
        if result.is_err() {
            pager
                .op(
                    native_runtime::native_abi::pager::OP_FAIL,
                    self.object.vmo(),
                    offset,
                    length,
                )
                .expect("failed memory page must retire its pager request");
        }
    }
}

impl MemoryWrite<'_> {
    pub(super) fn belongs_to(&self, file: &MemoryFile) -> bool {
        core::ptr::eq(self.file, file)
    }

    pub(super) fn size(&self) -> u64 {
        self.file.size()
    }

    pub(crate) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        self.file.write_locked(offset, input)
    }

    pub(crate) fn append(
        &self,
        input: &[u8],
        size_limit: u64,
    ) -> Result<(u64, usize), FileSystemError> {
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

enum MappingWrite {
    Private,
    SharedReadOnly,
    SharedWritable,
}

struct MemoryMapping {
    file: Arc<MemoryFile>,
    write: MappingWrite,
}

impl Drop for MemoryMapping {
    fn drop(&mut self) {
        if matches!(self.write, MappingWrite::SharedWritable) {
            assert!(
                self.file.writable_mappings.fetch_sub(1, Ordering::AcqRel) != 0,
                "mapping lease underflow"
            );
        }
    }
}

impl core::fmt::Debug for MemoryMapping {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MemoryMapping")
            .finish_non_exhaustive()
    }
}

impl SharedFileMapping for MemoryMapping {
    fn may_write(&self) -> bool {
        !matches!(self.write, MappingWrite::SharedReadOnly)
    }

    fn object(&self) -> Result<Arc<MemoryObject>, SharedFileError> {
        Ok(self.file.object.clone())
    }

    fn sync_range(&self, _offset: u64, _length: u64) -> Result<(), SharedFileError> {
        Ok(())
    }
}
