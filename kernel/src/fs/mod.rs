use alloc::sync::Arc;

pub(crate) mod ext2;
pub(crate) mod file;
pub(crate) mod inode;
pub(crate) mod vfs;

pub(crate) use ext2::Ext2FileSystem;
pub(crate) use file::{Console, FileDescriptorTable, OpenFileDescription, OpenFileKind};
pub(crate) use inode::{DirectoryEntry, Inode, InodeMetadata, InodeType};

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
    SymbolicLink,
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
