//! 持久文件的页缓存：每个缓存文件一个 pager VMO，它是文件内容的唯一驻留副本。
//!
//! # 读、写与映射
//!
//! 1. read/write 直接访问 VMO；缺失的页由内核向 [`pager`] 请求，pager 从存储读入后补入。
//! 2. mmap 直接映射同一 VMO（共享）或其写时复制子对象（私有），因此 read/write 与 mmap 看到同一页。
//! 3. 缓冲写（[`Inode::buffered_writes`]）只写 VMO，内核记录脏页，由回写交给存储；直写文件系统
//!    先写存储，再更新缓存并清除这些页的脏标记。
//!
//! # 锁
//!
//! - `write_sequence` 串行化一次完整的 regular write、truncate、allocate 与 fsync；
//! - `operation` 串行化改变存储内容或 EOF 的操作与回写。pager 补页不取这两把锁：内核在补页
//!   完成前阻塞访问者，补页读到的存储内容由持锁的修改者在修改后通过 VMO 写入覆盖。

use alloc::sync::Arc;

use native_runtime::native_abi::vmo;

use crate::memory::{MemoryObject, PAGE_SIZE, SharedFileError, SharedFileId, SharedFileMapping};
use crate::sync::{TaskMutex, TaskMutexGuard};

use super::memory_file::{MemoryFile, MemoryWrite};
use super::{AccessIdentity, DataBacking, FileSystemError, Inode, InodeType};

mod direct_write;
mod fill;
use super::pager;
mod registry;
mod regular_file;
mod regular_write;
mod remote_file;
mod writeback;
mod writeback_batch;

use registry::cached_file;
pub(super) use registry::evict_if_unlinked;
pub(crate) use registry::{
    evict_cached, evict_filesystem, invalidate, statistics, sync_all, write_back, writeback_cached,
};
pub(crate) use regular_file::RegularFile;

/// 页缓存的全局页统计。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageCacheStatistics {
    /// 已驻留的页。
    pub(crate) resident_pages: usize,
    /// 脏页。
    pub(crate) dirty_pages: usize,
}

/// 一个缓存文件。
pub(crate) struct CachedFile {
    id: SharedFileId,
    inode: Arc<dyn Inode>,
    object: Arc<MemoryObject>,
    /// pager 补页请求的 key。
    key: u64,
    operation: TaskMutex<()>,
    write_sequence: TaskMutex<()>,
}

impl core::fmt::Debug for CachedFile {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("CachedFile")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

fn status_error(status: native_runtime::native_abi::Status) -> FileSystemError {
    match status {
        native_runtime::native_abi::ERR_NO_MEMORY => FileSystemError::OutOfMemory,
        _ => FileSystemError::IoError,
    }
}

impl CachedFile {
    fn lock<'a>(
        &self,
        mutex: &'a TaskMutex<()>,
    ) -> Result<TaskMutexGuard<'a, ()>, FileSystemError> {
        mutex.lock().map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 让 VMO 覆盖到 `size`（按页取整）。
    fn cover(&self, size: u64) -> Result<(), FileSystemError> {
        let needed = size.next_multiple_of(PAGE_SIZE as u64);
        let current = self.object.size().map_err(|_| FileSystemError::IoError)?;
        if needed > current {
            self.object
                .set_size(needed)
                .map_err(|_| FileSystemError::OutOfMemory)?;
        }
        Ok(())
    }

    /// 读取 `[offset, offset + output.len())` 中 EOF 之前的部分。
    fn read(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        let size = self.inode.size();
        if offset >= size {
            return Ok(0);
        }
        let count = output.len().min((size - offset) as usize);
        self.cover(size)?;
        self.object
            .vmo()
            .read(&mut output[..count], offset)
            .map_err(status_error)?;
        Ok(count)
    }

    /// 缓冲写：只写入缓存，推进 EOF；脏页由回写交给存储。
    fn write_buffered(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        let end = offset
            .checked_add(input.len() as u64)
            .ok_or(FileSystemError::NoSpace)?;
        self.cover(end)?;
        self.object
            .vmo()
            .write(input, offset)
            .map_err(status_error)?;
        if end > self.inode.size() {
            self.inode.extend_buffered_size(end);
        }
        Ok(input.len())
    }

    /// 直写后把已提交到存储的字节同步进缓存，并清除这些页的脏标记（存储已是最新）。
    fn update_cached(&self, offset: u64, input: &[u8]) -> Result<(), FileSystemError> {
        if input.is_empty() {
            return Ok(());
        }
        let end = offset + input.len() as u64;
        self.cover(end)?;
        self.object
            .vmo()
            .write(input, offset)
            .map_err(status_error)?;
        let first = offset & !(PAGE_SIZE as u64 - 1);
        let last = end.next_multiple_of(PAGE_SIZE as u64);
        pager::mark_clean(&self.object, first, last - first);
        Ok(())
    }

    /// 丢弃 `[offset, offset + length)` 中的缓存页（之后访问重新从存储读入）。
    fn drop_range(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        let first = offset & !(PAGE_SIZE as u64 - 1);
        let size = self.object.size().map_err(|_| FileSystemError::IoError)?;
        // 全文件失效用 u64::MAX 长度；先裁剪到 page-aligned VMO 大小，避免向上取整溢出成 0。
        let end = offset
            .saturating_add(length)
            .min(size)
            .next_multiple_of(PAGE_SIZE as u64);
        if first >= end {
            return Ok(());
        }
        self.object
            .vmo()
            .op_range(vmo::OP_DECOMMIT, first, end - first)
            .map_err(|status| {
                if status == native_runtime::native_abi::ERR_BAD_STATE {
                    FileSystemError::Busy
                } else {
                    status_error(status)
                }
            })
    }
}

impl SharedFileMapping for CachedFile {
    fn object(&self) -> Result<Arc<MemoryObject>, SharedFileError> {
        self.cover(self.inode.size()).map_err(fs_error)?;
        Ok(self.object.clone())
    }

    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError> {
        let _operation = self.lock(&self.operation).map_err(fs_error)?;
        self.writeback_range(offset, length, true).map_err(fs_error)
    }
}

/// 持有单 inode 写串行化权的一次 regular-file 修改；Drop 无条件释放。
pub(crate) enum RegularFileWrite<'a> {
    Cached(CachedWrite<'a>),
    Direct(DirectWrite<'a>),
    Memory(MemoryInodeWrite<'a>),
    Remote(remote_file::RemoteWrite<'a>),
}

/// 同一缓存写串行化下的 per-open 直接写（FUSE direct_io）。
pub(crate) struct DirectWrite<'a> {
    cached: CachedWrite<'a>,
    io: &'a dyn super::DirectIo,
}

/// 页缓存文件的一次写串行化权。
pub(crate) struct CachedWrite<'a> {
    file: &'a CachedFile,
    identity: AccessIdentity,
    _sequence: TaskMutexGuard<'a, ()>,
}

/// 内存型文件的一次写：同时持 inode 元数据与内容所有者。
pub(crate) struct MemoryInodeWrite<'a> {
    write: MemoryWrite<'a>,
    inode: &'a Arc<dyn Inode>,
    identity: AccessIdentity,
}

/// 文件映射的后备：同时保活实际打开的文件对象（Linux `vma->vm_file`）。
///
/// # Errors
///
/// 非 regular file 或快照文件返回 `InvalidOperation`；分配失败返回 `OutOfMemory`。
pub(crate) fn mapping(
    inode: Arc<dyn Inode>,
    owner: Option<Arc<dyn crate::memory::MappedFileOwner>>,
    may_write: bool,
    shared: bool,
    write: bool,
) -> Result<Arc<dyn SharedFileMapping>, FileSystemError> {
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return Err(FileSystemError::InvalidOperation);
    }
    let inner: Arc<dyn SharedFileMapping> = match inode.data_backing() {
        DataBacking::PageCache => cached_file(inode)?.ok_or(FileSystemError::InvalidOperation)?,
        DataBacking::Memory(file) => file.mapping(shared, may_write, write)?,
        DataBacking::Remote => remote_file::remote_file(inode)?,
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
    fn may_write(&self) -> bool {
        self.may_write && self.inner.may_write()
    }

    fn object(&self) -> Result<Arc<MemoryObject>, SharedFileError> {
        self.inner.object()
    }

    fn sync_range(&self, offset: u64, length: u64) -> Result<(), SharedFileError> {
        self.inner.sync_range(offset, length)
    }
}

/// 截断 regular file：先截断存储，再缩小缓存并清零新末页尾部。
///
/// # Errors
///
/// 存储截断失败。
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
        DataBacking::Memory(_) | DataBacking::Snapshot => {
            return inode.truncate_storage(size, Some(identity));
        }
        DataBacking::Remote => {
            // 服务调整自己的内容 VMO；写串行化防止与并发 append 交错。
            let file = remote_file::remote_file(inode)?;
            let _sequence = file
                .write_sequence
                .lock()
                .map_err(|_| FileSystemError::OutOfMemory)?;
            return file.inode.truncate_storage(size, Some(identity));
        }
    };
    let _sequence = file.lock(&file.write_sequence)?;
    let _operation = file.lock(&file.operation)?;
    let previous = file.inode.size();
    // 缩小前先回写区间外的脏页没有意义；区间内的脏页在截断后仍需写回，保留脏标记。
    file.inode.truncate_storage(size, Some(identity))?;
    let rounded = size.next_multiple_of(PAGE_SIZE as u64);
    if file.object.size().map_err(|_| FileSystemError::IoError)? > rounded {
        file.object
            .set_size(rounded)
            .map_err(|_| FileSystemError::IoError)?;
    }
    if size < previous && size < rounded {
        // 新末页中越过 EOF 的字节此后必须读到零；存储已截断，这些字节不需要回写。
        file.object
            .vmo()
            .op_range(vmo::OP_ZERO, size, rounded - size)
            .map_err(status_error)?;
    }
    Ok(())
}

/// 预分配存储块（`fallocate`）；缓存内容不变。
///
/// # Errors
///
/// 存储分配失败。
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
        DataBacking::Memory(_) | DataBacking::Remote => {
            return inode.allocate_storage(offset, length, Some(identity));
        }
        DataBacking::Snapshot => return Err(FileSystemError::ReadOnly),
    };
    let _sequence = file.lock(&file.write_sequence)?;
    let _operation = file.lock(&file.operation)?;
    file.inode.allocate_storage(offset, length, Some(identity))
}

/// `fsync`：写回全部脏页并让存储持久化。
///
/// # Errors
///
/// 写回或存储同步失败。
pub(crate) fn sync_inode(inode: Arc<dyn Inode>) -> Result<(), FileSystemError> {
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return inode.sync_storage();
    }
    let block_device = inode.inode_type() == InodeType::BlockDevice;
    let file = match inode.data_backing() {
        DataBacking::PageCache => cached_file(inode)?.ok_or(FileSystemError::InvalidOperation)?,
        DataBacking::Memory(_) | DataBacking::Snapshot => return Ok(()),
        // 服务写回映射写入并提交日志。
        DataBacking::Remote => return inode.sync_storage(),
    };
    let _sequence = file.lock(&file.write_sequence)?;
    let _operation = file.lock(&file.operation)?;
    file.writeback_range(0, u64::MAX, true)?;
    if block_device {
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
