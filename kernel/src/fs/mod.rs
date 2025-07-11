use alloc::sync::Arc;

use crate::drivers::BlockDevice;

pub mod fat32;
pub mod inode;
pub mod vfs;

pub use fat32::FAT32FileSystem;
pub use inode::{Inode, InodeType};
pub use vfs::get_vfs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSystemError {
    NotFound,
    AlreadyExists,
    NotDirectory,
    IsDirectory,
    InvalidPath,
    NoSpace,
    PermissionDenied,
    IoError,
    InvalidFileSystem,
}

pub trait FileSystem: Send + Sync {
    fn root_inode(&self) -> Arc<dyn Inode>;
    fn create_file(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn create_directory(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<Arc<dyn Inode>, FileSystemError>;
    fn remove(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<(), FileSystemError>;
    fn stat(&self, inode: &Arc<dyn Inode>) -> Result<FileStat, FileSystemError>;
    fn sync(&self) -> Result<(), FileSystemError>;
}

#[derive(Debug, Clone, Copy)]
pub struct FileStat {
    pub size: u64,
    pub file_type: InodeType,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
}

impl Default for FileStat {
    fn default() -> Self {
        Self {
            size: 0,
            file_type: InodeType::File,
            mode: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
        }
    }
}

pub fn make_filesystem(device: Arc<dyn BlockDevice>) -> Option<Arc<dyn FileSystem>> {
    if let Some(fs) = FAT32FileSystem::new(device.clone()) {
        Some(fs)
    } else {
        None
    }
}