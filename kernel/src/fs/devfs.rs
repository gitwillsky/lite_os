use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

use super::{FileStat, FileSystem, FileSystemError, Inode, InodeType};

/// devfs: 虚拟设备文件系统
pub struct DevFileSystem;

impl DevFileSystem {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl FileSystem for DevFileSystem {
    fn root_inode(&self) -> Arc<dyn Inode> {
        Arc::new(DevRoot {}) as Arc<dyn Inode>
    }
    fn create_file(
        &self,
        _parent: &Arc<dyn Inode>,
        _name: &str,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn create_directory(
        &self,
        _parent: &Arc<dyn Inode>,
        _name: &str,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn remove(&self, _parent: &Arc<dyn Inode>, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn stat(&self, inode: &Arc<dyn Inode>) -> Result<FileStat, FileSystemError> {
        Ok(FileStat {
            size: inode.size(),
            file_type: inode.inode_type(),
            mode: match inode.inode_type() {
                InodeType::Directory => 0o755,
                InodeType::File | InodeType::Device | InodeType::Fifo | InodeType::SymLink => 0o644,
            },
            ..FileStat::default()
        })
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}

struct DevRoot;
impl Inode for DevRoot {
    fn inode_type(&self) -> InodeType {
        InodeType::Directory
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        Ok(vec!["input".to_string()])
    }
    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        match name {
            "" | "." => Ok(Arc::new(DevRoot {}) as Arc<dyn Inode>),
            "input" => Ok(Arc::new(DevDirInput {}) as Arc<dyn Inode>),
            _ => Err(FileSystemError::NotFound),
        }
    }
    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}

struct DevDirInput;
impl Inode for DevDirInput {
    fn inode_type(&self) -> InodeType {
        InodeType::Directory
    }
    fn size(&self) -> u64 {
        0
    }
    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }
    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        // 从驱动注册表枚举 /dev/input/eventX
        let nodes = crate::drivers::virtio_input::list_input_nodes();
        let mut entries: Vec<String> = Vec::new();
        for p in nodes {
            if let Some(name) = p.strip_prefix("/dev/input/") {
                entries.push(name.to_string());
            }
        }
        Ok(entries)
    }
    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let full = alloc::format!("/dev/input/{}", name);
        crate::drivers::open_input_device(&full)
    }
    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}
