use alloc::{sync::Arc, vec::Vec};

use super::FileSystemError;

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InodeType {
    File = 0,
    Directory = 1,
    SymLink = 2,
    Fifo = 4,
}

/// @description VFS 与 Linux stat/getdents 共享的稳定 inode 元数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InodeMetadata {
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) mode: u32,
    pub(crate) links: u32,
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(crate) size: u64,
    pub(crate) blocks: u64,
    pub(crate) block_size: u32,
    pub(crate) atime: u64,
    pub(crate) mtime: u64,
    pub(crate) ctime: u64,
}

/// @description 一个目录项的原始字节名称与 inode identity。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryEntry {
    pub(crate) inode: u64,
    pub(crate) kind: InodeType,
    pub(crate) name: Vec<u8>,
}

/// @description 唯一 VFS inode 接口，读写和目录变更不保留只读旁路。
pub(crate) trait Inode: Send + Sync {
    fn filesystem_id(&self) -> usize;

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError>;

    fn inode_type(&self) -> InodeType;

    fn size(&self) -> u64;

    fn is_executable(&self) -> bool;

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError>;

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError>;

    fn append(&self, buf: &[u8]) -> Result<(u64, usize), FileSystemError>;

    fn truncate(&self, size: u64) -> Result<(), FileSystemError>;

    fn sync(&self) -> Result<(), FileSystemError>;

    fn list(&self) -> Result<Vec<DirectoryEntry>, FileSystemError>;

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError>;

    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        mode: u32,
    ) -> Result<Arc<dyn Inode>, FileSystemError>;

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError>;

    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError>;
}
