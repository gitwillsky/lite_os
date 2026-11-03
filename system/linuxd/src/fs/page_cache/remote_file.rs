//! 内容由文件系统服务拥有的文件（[`DataBacking::Remote`]，Starnix RemoteFs）：读与 mmap 使用服务交出的
//! VMO，写经 inode 的存储接口由服务直写并同步该 VMO。linuxd 不缓存它的页，也不回写。
//!
//! 同一文件只有一个 [`RemoteFile`]：多个打开与映射共用一个 VMO 与写串行化（追加写需要大小与写入原子）。

use super::*;
use crate::sync::Mutex;
use alloc::sync::Weak;
use fallible_tree::FallibleMap;

/// 一个远端内容文件。
pub(crate) struct RemoteFile {
    pub(super) id: SharedFileId,
    pub(super) inode: Arc<dyn Inode>,
    pub(super) memory: Arc<MemoryObject>,
    // OWNER: 串行化完整的 regular write（含 append 的大小读取）；缺失时并发 append 写到同一偏移。
    pub(super) write_sequence: TaskMutex<()>,
}

// OWNER: 存活远端内容文件的唯一索引；Weak 由打开与映射保活，缺失时同一文件出现两份写串行化。
static REMOTES: Mutex<FallibleMap<SharedFileId, Weak<RemoteFile>>> = Mutex::new(FallibleMap::new());

/// 取得或建立 `inode` 的远端内容文件。
///
/// # Errors
///
/// 服务交出 VMO 失败或分配失败。
pub(crate) fn remote_file(inode: Arc<dyn Inode>) -> Result<Arc<RemoteFile>, FileSystemError> {
    let id = inode.page_cache_id()?;
    if let Some(file) = REMOTES.lock().get(&id).and_then(Weak::upgrade) {
        return Ok(file);
    }
    let memory =
        MemoryObject::new(inode.backing_memory()?).map_err(|_| FileSystemError::OutOfMemory)?;
    let candidate = Arc::try_new(RemoteFile {
        id,
        inode,
        memory,
        write_sequence: TaskMutex::new(()),
    })
    .map_err(|_| FileSystemError::OutOfMemory)?;
    let mut remotes = REMOTES.lock();
    if let Some(existing) = remotes.get(&id).and_then(Weak::upgrade) {
        return Ok(existing);
    }
    remotes.remove(&id);
    remotes
        .try_insert(id, Arc::downgrade(&candidate))
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(candidate)
}

impl RemoteFile {
    /// 读取 `[offset, offset + output.len())` 中 EOF 之前的部分；缺页由服务补入。
    pub(super) fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let size = self
            .inode
            .size()
            .min(self.memory.size().map_err(|_| FileSystemError::IoError)?);
        if offset >= size {
            return Ok(0);
        }
        let count = output.len().min((size - offset) as usize);
        self.memory
            .vmo()
            .read(&mut output[..count], offset)
            .map_err(status_error)?;
        Ok(count)
    }
}

impl core::fmt::Debug for RemoteFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("RemoteFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl Drop for RemoteFile {
    fn drop(&mut self) {
        let mut remotes = REMOTES.lock();
        if remotes
            .get(&self.id)
            .is_some_and(|entry| entry.strong_count() == 0)
        {
            remotes.remove(&self.id);
        }
    }
}

impl SharedFileMapping for RemoteFile {
    fn object(&self) -> Result<Arc<MemoryObject>, SharedFileError> {
        Ok(self.memory.clone())
    }

    fn sync_range(&self, _offset: u64, _length: u64) -> Result<(), SharedFileError> {
        // 服务拥有映射写入的回写：fsync 让它写回全部脏页并提交日志。
        self.inode.sync_storage().map_err(fs_error)
    }
}

/// 持有远端文件写串行化权的一次 regular write。
pub(crate) struct RemoteWrite<'a> {
    pub(super) file: &'a RemoteFile,
    pub(super) identity: AccessIdentity,
    pub(super) _sequence: TaskMutexGuard<'a, ()>,
}

impl RemoteWrite<'_> {
    pub(super) fn write(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        self.file
            .inode
            .write_storage(offset, input, Some(&self.identity))
    }

    pub(super) fn append(&self, input: &[u8], limit: u64) -> Result<(u64, usize), FileSystemError> {
        let offset = self.file.inode.size();
        let count = usize::try_from(limit.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(input.len());
        self.write(offset, &input[..count])
            .map(|written| (offset, written))
    }
}
