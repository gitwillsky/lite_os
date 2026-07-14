use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};

mod devfs;
mod epoll;
mod ext2;
mod file;
mod inode;
mod page_cache;
mod permission;
mod procfs;
mod sysfs;
mod vfs;

pub(crate) use devfs::DevFileSystem;
pub(crate) use epoll::{Epoll, EpollChange, EpollChangeError, EpollEvent};
pub(crate) use ext2::Ext2FileSystem;
pub(crate) use file::{
    CharacterDevice, Console, FileDescriptorError, FileDescriptorTable, MAX_FILE_DESCRIPTORS,
    O_ACCMODE, O_APPEND, O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_RDWR, O_WRONLY, OpenFileDescription,
    OpenFileKind, Terminal, TerminalAccess, TerminalRead, TerminalReadMode,
};
pub(crate) use inode::{DeviceKind, DirectoryEntry, Inode, InodeMetadata, InodeType};
pub(crate) use page_cache::{
    RegularFile, RegularFileWrite, allocate, mapping, statistics as page_cache_statistics,
    sync_all, sync_inode, truncate,
};
pub(crate) use permission::{AccessIdentity, CreateMetadata};
pub(crate) use procfs::{
    ProcCpuSnapshot, ProcFileDescriptorSnapshot, ProcFileSystem, ProcIoSnapshot,
    ProcNetworkSnapshot, ProcProcessSnapshot, ProcSnapshot, ProcSource, ProcThreadSnapshot,
};
pub(crate) use sysfs::SysFileSystem;
pub(crate) use vfs::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, OpenedFile, RecordLockMode, RecordLockRange, init as init_vfs, vfs,
};

/// @description filesystem adapter 向 VFS 投影的容量、inode 与类型快照。
pub(crate) struct FileSystemStatistics {
    /// `/proc/mounts` 使用的 filesystem type name。
    pub(crate) type_name: &'static str,
    /// Linux `statfs.f_type` magic。
    pub(crate) magic: u64,
    /// 最优传输块大小。
    pub(crate) block_size: u64,
    /// 可供数据使用的总块数。
    pub(crate) blocks: u64,
    /// 包含 reserved blocks 的空闲块数。
    pub(crate) blocks_free: u64,
    /// 非特权调用者可用的空闲块数。
    pub(crate) blocks_available: u64,
    /// 总 inode 数。
    pub(crate) files: u64,
    /// 空闲 inode 数。
    pub(crate) files_free: u64,
    /// filesystem instance 的稳定标识。
    pub(crate) fsid: [u32; 2],
    /// 单个 pathname component 的最大字节数。
    pub(crate) name_length: u64,
    /// 容量计数使用的基本块大小。
    pub(crate) fragment_size: u64,
    /// Linux `ST_*` flags；VFS 负责补充 `ST_VALID`。
    pub(crate) flags: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileSystemError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    DirectoryNotEmpty,
    InvalidPath,
    IoError,
    InvalidFileSystem,
    InvalidOperation,
    ReadOnly,
    SymbolicLink,
    OutOfMemory,
    NoSpace,
    CrossDevice,
    PermissionDenied,
    AccessDenied,
    Busy,
    TooManyLinks,
}

struct FallibleBytes(Vec<u8>);

impl Write for FallibleBytes {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.0.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

fn try_format_bytes(arguments: fmt::Arguments<'_>) -> Result<Vec<u8>, FileSystemError> {
    let mut bytes = FallibleBytes(Vec::new());
    bytes
        .write_fmt(arguments)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    Ok(bytes.0)
}

/// @description 为 VFS 提供根 inode 的文件系统实例。
pub(crate) trait FileSystem: Send + Sync {
    /// 加载该文件系统的根 inode。
    ///
    /// # Returns
    ///
    /// 指向根目录 inode 的共享引用。
    ///
    /// # Errors
    ///
    /// 根 inode 无法从磁盘读取或数据无效时返回错误。
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError>;

    /// @description 取得一次 filesystem-owned 容量与 inode 统计快照。
    ///
    /// @return 当前统计；不得缓存或从 VFS/syscall 反向推导。
    fn statistics(&self) -> FileSystemStatistics;
}
