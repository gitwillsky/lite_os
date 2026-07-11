use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{FileSystem, FileSystemError, Inode, InodeType};

/// @description 管理唯一根文件系统，并为内核 ELF 加载器解析绝对路径。
pub struct VirtualFileSystem {
    root_fs: Mutex<Option<Arc<dyn FileSystem>>>,
}

impl VirtualFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.root_fs
            .lock()
            .as_ref()
            .ok_or(FileSystemError::NotFound)?
            .root_inode()
    }

    fn resolve_from(
        &self,
        start: Arc<dyn Inode>,
        path: &[u8],
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let root = self.root_inode()?;
        let root_identity = (root.filesystem_id(), root.metadata()?.inode);
        let mut inode = if path.first() == Some(&b'/') {
            root
        } else {
            start
        };
        for component in path.split(|byte| *byte == b'/') {
            match component {
                b"" | b"." => {}
                b".." => {
                    if (inode.filesystem_id(), inode.metadata()?.inode) != root_identity {
                        inode = inode.find_child(b"..")?;
                    }
                }
                name => {
                    inode = inode.find_child(name)?;
                    if inode.inode_type() == InodeType::SymLink {
                        return Err(FileSystemError::SymbolicLink);
                    }
                }
            }
        }
        if path.len() > 1
            && path.last() == Some(&b'/')
            && inode.inode_type() != InodeType::Directory
        {
            return Err(FileSystemError::NotDirectory);
        }
        Ok(inode)
    }

    fn parent_from(
        &self,
        start: Arc<dyn Inode>,
        path: &[u8],
    ) -> Result<(Arc<dyn Inode>, Vec<u8>), FileSystemError> {
        let trimmed = path.strip_suffix(b"/").unwrap_or(path);
        let split = trimmed.iter().rposition(|byte| *byte == b'/');
        let (parent_path, name) = match split {
            Some(0) => (&b"/"[..], &trimmed[1..]),
            Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
            None => (&b"."[..], trimmed),
        };
        if name.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }
        Ok((self.resolve_from(start, parent_path)?, name.to_vec()))
    }

    /// 创建尚未挂载根文件系统的 VFS。
    ///
    /// # Returns
    ///
    /// 空的 VFS 实例。
    pub fn new() -> Self {
        Self {
            root_fs: Mutex::new(None),
        }
    }

    /// 挂载唯一的根文件系统。
    ///
    /// # Parameters
    ///
    /// - `fs`: 根文件系统实例。
    ///
    /// # Returns
    ///
    /// 首次挂载成功时返回 `()`。
    ///
    /// # Errors
    ///
    /// 根文件系统已挂载时返回 `AlreadyExists`，防止静默替换启动卷。
    pub fn mount_root(&self, fs: Arc<dyn FileSystem>) -> Result<(), FileSystemError> {
        let mut root_fs = self.root_fs.lock();
        if root_fs.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        *root_fs = Some(fs);
        Ok(())
    }

    /// 从根文件系统打开一个内核可见 inode。
    ///
    /// # Parameters
    ///
    /// - `path`: 必须以 `/` 开头的 NUL 之前原始路径字节。
    ///
    /// # Returns
    ///
    /// 成功时返回解析后 inode 的共享引用。
    ///
    /// # Errors
    ///
    /// 路径非绝对路径、根文件系统未挂载、分量不存在、遇到符号链接，
    /// 或者带尾随 `/` 的结果不是目录时返回错误。
    pub fn open(&self, path: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if path.first() != Some(&b'/') {
            return Err(FileSystemError::InvalidPath);
        }
        self.resolve_from(self.root_inode()?, path)
    }

    pub fn open_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        self.resolve_from(start, path)
    }

    pub fn create_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        kind: InodeType,
        mode: u32,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path)?;
        parent.create(&name, kind, mode)
    }

    pub fn unlink_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        directory: bool,
    ) -> Result<(), FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path)?;
        parent.unlink(&name, directory)
    }

    pub fn rename_at(
        &self,
        old_start: Option<Arc<dyn Inode>>,
        old_path: &[u8],
        new_start: Option<Arc<dyn Inode>>,
        new_path: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        let old_start = old_start.unwrap_or(self.root_inode()?);
        let new_start = new_start.unwrap_or(self.root_inode()?);
        let (old_parent, old_name) = self.parent_from(old_start, old_path)?;
        let (new_parent, new_name) = self.parent_from(new_start, new_path)?;
        if old_parent.filesystem_id() != new_parent.filesystem_id() {
            return Err(FileSystemError::InvalidOperation);
        }
        old_parent.rename(
            &old_name,
            new_parent.metadata()?.inode,
            &new_name,
            no_replace,
        )
    }
}

use spin::Once;

pub static VFS_MANAGER: Once<VirtualFileSystem> = Once::new();

pub fn init() {
    VFS_MANAGER.call_once(VirtualFileSystem::new);
}

pub fn vfs() -> &'static VirtualFileSystem {
    VFS_MANAGER.wait()
}
