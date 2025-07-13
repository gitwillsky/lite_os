use alloc::{string::String, sync::Arc, vec::Vec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeType {
    File,
    Directory,
    SymLink,
    Device,
}

pub trait Inode: Send + Sync {
    fn inode_type(&self) -> InodeType;
    fn size(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, super::FileSystemError>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, super::FileSystemError>;
    fn list_dir(&self) -> Result<Vec<String>, super::FileSystemError>;
    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, super::FileSystemError>;
    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, super::FileSystemError>;
    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, super::FileSystemError>;
    fn remove(&self, name: &str) -> Result<(), super::FileSystemError>;
    fn truncate(&self, size: u64) -> Result<(), super::FileSystemError>;
    fn sync(&self) -> Result<(), super::FileSystemError>;
}