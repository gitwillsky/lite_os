//! 内存型文件（tmpfs、memfd）：内容就是一个可调整大小的 VMO，没有后端存储。
//!
//! 1. 文件长度单独发布；VMO 大小是长度按页取整，页在首次写入时由内核提交。
//! 2. 配额（`StorageBudget`）按逻辑长度的页数预留：增长前预留、截断后归还。
//! 3. 共享映射直接映射该 VMO，因此 read/write 与 mmap 看到同一页；截断缩小 VMO 时内核撤销越界映射。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use native_runtime::native_abi::vmo;

use super::{FileSystemError, StorageBudget};
use crate::memory::{MemoryObject, PAGE_SIZE, SharedFileError, SharedFileId, SharedFileMapping};
use crate::sync::{TaskMutex, TaskMutexGuard};

mod seals;
use seals::{SealError, Seals};

struct State {
    seals: Seals,
    /// 已向配额预留的页数（等于逻辑长度按页取整）。
    reserved_pages: u64,
}

/// tmpfs/memfd 文件内容。
pub(crate) struct MemoryFile {
    id: SharedFileId,
    budget: Arc<StorageBudget>,
    object: Arc<MemoryObject>,
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
    pub(super) fn new(id: SharedFileId, budget: Arc<StorageBudget>, allow_sealing: bool) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            id,
            budget,
            object: MemoryObject::anonymous(0, true).map_err(object_error)?,
            write_sequence: TaskMutex::new(()),
            state: TaskMutex::new(State {
                seals: Seals::new(allow_sealing),
                reserved_pages: 0,
            }),
            writable_mappings: AtomicUsize::new(0),
            length: AtomicU64::new(0),
            times: super::inode_times::InodeTimes::new()?,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    fn state(&self) -> Result<TaskMutexGuard<'_, State>, FileSystemError> {
        self.state.lock().map_err(|_| FileSystemError::OutOfMemory)
    }

    fn sequence(&self) -> Result<TaskMutexGuard<'_, ()>, FileSystemError> {
        self.write_sequence.lock().map_err(|_| FileSystemError::OutOfMemory)
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
        let length = self.size();
        if offset >= length {
            return Ok(0);
        }
        let count = output.len().min((length - offset) as usize);
        self.object
            .vmo()
            .read(&mut output[..count], offset)
            .map_err(|_| FileSystemError::IoError)?;
        Ok(count)
    }

    pub(crate) fn begin_write(&self) -> Result<MemoryWrite<'_>, FileSystemError> {
        Ok(MemoryWrite {
            file: self,
            _sequence: self.sequence()?,
        })
    }

    /// 把逻辑长度改为 `length`：调整配额预留与 VMO 大小；缩小时清零新末页的尾部。
    /// 调用者持有写串行化锁。
    fn resize_locked(&self, state: &mut State, length: u64) -> Result<(), FileSystemError> {
        let pages = pages_for(length);
        while state.reserved_pages < pages {
            if !self.budget.reserve_page() {
                self.release_extra(state, pages_for(self.size()));
                return Err(FileSystemError::NoSpace);
            }
            state.reserved_pages += 1;
        }
        let vmo_size = pages * PAGE_SIZE as u64;
        if self.object.size().map_err(object_error)? != vmo_size
            && let Err(error) = self.object.set_size(vmo_size)
        {
            self.release_extra(state, pages_for(self.size()));
            return Err(object_error(error));
        }
        if length < vmo_size {
            // 页内越过长度的字节必须读到零（之后增长时不能重新暴露旧内容）。
            self.object
                .vmo()
                .op_range(vmo::OP_ZERO, length, vmo_size - length)
                .map_err(|_| FileSystemError::IoError)?;
        }
        self.release_extra(state, pages);
        self.length.store(length, Ordering::Release);
        Ok(())
    }

    fn release_extra(&self, state: &mut State, pages: u64) {
        while state.reserved_pages > pages {
            self.budget.release_page();
            state.reserved_pages -= 1;
        }
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
            self.resize_locked(&mut state, end)?;
        }
        if let Err(status) = self.object.vmo().write(input, offset) {
            if end > length {
                let _ = self.resize_locked(&mut state, length);
            }
            return Err(if status == native_runtime::native_abi::ERR_NO_MEMORY {
                FileSystemError::OutOfMemory
            } else {
                FileSystemError::IoError
            });
        }
        drop(state);
        self.times.touch();
        Ok(input.len())
    }

    pub(super) fn truncate_with_metadata<G>(
        &self,
        size: u64,
        prepare: impl FnOnce() -> Result<G, FileSystemError>,
        commit: impl FnOnce(&mut G),
    ) -> Result<(), FileSystemError> {
        let _sequence = self.sequence()?;
        let mut state = self.state()?;
        state.seals.check_truncate(self.size(), size).map_err(seal_error)?;
        let mut metadata = prepare()?;
        self.resize_locked(&mut state, size)?;
        drop(state);
        commit(&mut metadata);
        self.times.touch();
        Ok(())
    }

    pub(super) fn allocate_with_metadata(&self, offset: u64, length: u64, modified: impl FnOnce()) -> Result<(), FileSystemError> {
        let _sequence = self.sequence()?;
        let end = offset.checked_add(length).ok_or(FileSystemError::InvalidOperation)?;
        let mut state = self.state()?;
        let current = self.size();
        state.seals.check_growth(current, end).map_err(seal_error)?;
        if end > current {
            self.resize_locked(&mut state, end)?;
        }
        let start = offset & !(PAGE_SIZE as u64 - 1);
        let commit_end = end.next_multiple_of(PAGE_SIZE as u64);
        self.object
            .vmo()
            .op_range(vmo::OP_COMMIT, start, commit_end - start)
            .map_err(|_| FileSystemError::NoSpace)?;
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
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |count| count.checked_add(1))
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
        let state = self.state.get_mut();
        while state.reserved_pages > 0 {
            self.budget.release_page();
            state.reserved_pages -= 1;
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

    pub(crate) fn append(&self, input: &[u8], size_limit: u64) -> Result<(u64, usize), FileSystemError> {
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
            assert!(self.file.writable_mappings.fetch_sub(1, Ordering::AcqRel) != 0, "mapping lease underflow");
        }
    }
}

impl core::fmt::Debug for MemoryMapping {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("MemoryMapping").finish_non_exhaustive()
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
