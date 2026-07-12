use alloc::sync::Arc;

mod devfs;
mod ext2;
mod file;
mod inode;
mod procfs;
mod vfs;

pub(crate) use devfs::DevFileSystem;
pub(crate) use ext2::Ext2FileSystem;
pub(crate) use file::{
    CharacterDevice, Console, FileDescriptorTable, MAX_FILE_DESCRIPTORS, O_ACCMODE, O_APPEND,
    O_CLOEXEC, O_NONBLOCK, O_RDONLY, O_WRONLY, OpenFileDescription, OpenFileKind, Terminal,
    TerminalRead,
};
pub(crate) use inode::{DeviceKind, DirectoryEntry, Inode, InodeMetadata, InodeType};
pub(crate) use procfs::{
    ProcCpuSnapshot, ProcFileSystem, ProcProcessSnapshot, ProcSnapshot, ProcSource,
};
pub(crate) use vfs::{init as init_vfs, vfs};

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
}
