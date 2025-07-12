use alloc::{collections::BTreeMap, format, string::{String, ToString}, sync::Arc};
use spin::Mutex;

use crate::task;

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

    /// 将相对路径转换为绝对路径
    pub fn resolve_relative_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            // 已经是绝对路径
            path.to_string()
        } else {
            // 相对路径：结合当前工作目录
            let cwd = task::current_cwd();
            if cwd.ends_with('/') {
                format!("{}{}", cwd, path)
            } else {
                format!("{}/{}", cwd, path)
            }
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
        let abs_path = self.resolve_relative_path(path);
        
        if abs_path == "/" {
            let root_fs = self.root_fs.lock();
            let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;
            return Ok(fs.root_inode());
        }
        
        self.resolve_path(&abs_path)
    }

    fn resolve_path(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let root_fs = self.root_fs.lock();
        let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;
        
        let mut current = fs.root_inode();
        
        let path = if path.starts_with('/') {
            &path[1..] // Remove leading '/'
        } else {
            path // Treat relative paths as relative to root
        };
        
        if path.is_empty() {
            return Ok(current);
        }
        
        for component in path.split('/') {
            if component.is_empty() {
                continue;
            }
            current = current.find_child(component)?;
        }
        
        Ok(current)
    }

    pub fn create_file(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, filename) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.create_file(&filename)
    }

    pub fn create_directory(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, dirname) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.create_directory(&dirname)
    }

    pub fn remove(&self, path: &str) -> Result<(), FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, filename) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.remove(&filename)
    }

    fn split_path(&self, path: &str) -> Result<(String, String), FileSystemError> {
        if !path.starts_with('/') {
            return Err(FileSystemError::InvalidPath);
        }
        
        let path = &path[1..];
        if path.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }
        
        if let Some(pos) = path.rfind('/') {
            let parent_path = format!("/{}", &path[..pos]);
            let filename = path[pos + 1..].to_string();
            if filename.is_empty() {
                return Err(FileSystemError::InvalidPath);
            }
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