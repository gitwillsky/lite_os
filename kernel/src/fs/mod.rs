use alloc::sync::Arc;

pub mod ext2;
pub mod inode;
pub mod vfs;

pub use ext2::Ext2FileSystem;
pub use inode::{Inode, InodeType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystemError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    InvalidPath,
    IoError,
    InvalidFileSystem,
    InvalidOperation,
}

/// @description 为 VFS 提供根 inode 的文件系统实例。
pub trait FileSystem: Send + Sync {
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
