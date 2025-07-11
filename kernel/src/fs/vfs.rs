use alloc::{collections::BTreeMap, format, string::{String, ToString}, sync::Arc};
use spin::Mutex;

use super::{FileSystem, FileSystemError, Inode};

pub struct VirtualFileSystem {
    filesystems: Mutex<BTreeMap<String, Arc<dyn FileSystem>>>,
    root_fs: Mutex<Option<Arc<dyn FileSystem>>>,
}

impl VirtualFileSystem {
    pub fn new() -> Self {
        Self {
            filesystems: Mutex::new(BTreeMap::new()),
            root_fs: Mutex::new(None),
        }
    }

    pub fn mount(&self, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), FileSystemError> {
        let mut filesystems = self.filesystems.lock();
        
        if path == "/" {
            *self.root_fs.lock() = Some(fs.clone());
        }
        
        filesystems.insert(path.to_string(), fs);
        Ok(())
    }

    pub fn unmount(&self, path: &str) -> Result<(), FileSystemError> {
        let mut filesystems = self.filesystems.lock();
        
        if path == "/" {
            *self.root_fs.lock() = None;
        }
        
        filesystems.remove(path);
        Ok(())
    }

    pub fn open(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let root_fs = self.root_fs.lock();
        let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;
        
        if path == "/" {
            return Ok(fs.root_inode());
        }
        
        self.resolve_path(path)
    }

    fn resolve_path(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let root_fs = self.root_fs.lock();
        let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;
        
        let mut current = fs.root_inode();
        
        if path.starts_with('/') {
            let path = &path[1..]; // 去掉开头的'/'
            if path.is_empty() {
                return Ok(current);
            }
            
            for component in path.split('/') {
                if component.is_empty() {
                    continue;
                }
                current = current.find_child(component)?;
            }
        } else {
            return Err(FileSystemError::InvalidPath);
        }
        
        Ok(current)
    }

    pub fn create_file(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let (parent_path, filename) = self.split_path(path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.create_file(&filename)
    }

    pub fn create_directory(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let (parent_path, dirname) = self.split_path(path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.create_directory(&dirname)
    }

    pub fn remove(&self, path: &str) -> Result<(), FileSystemError> {
        let (parent_path, filename) = self.split_path(path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.remove(&filename)
    }

    fn split_path(&self, path: &str) -> Result<(String, String), FileSystemError> {
        if !path.starts_with('/') {
            return Err(FileSystemError::InvalidPath);
        }
        
        let path = &path[1..];
        if let Some(pos) = path.rfind('/') {
            let parent_path = format!("/{}", &path[..pos]);
            let filename = path[pos + 1..].to_string();
            Ok((parent_path, filename))
        } else {
            Ok(("/".to_string(), path.to_string()))
        }
    }
}

use spin::Once;

pub static VFS_MANAGER: Once<VirtualFileSystem> = Once::new();

pub fn init_vfs() {
    VFS_MANAGER.call_once(|| VirtualFileSystem::new());
}

pub fn get_vfs() -> &'static VirtualFileSystem {
    VFS_MANAGER.wait()
}