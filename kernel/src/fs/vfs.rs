use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{FileSystem, FileSystemError, Inode, InodeType};

/// @description 管理唯一根文件系统，并为内核 ELF 加载器解析绝对路径。
pub struct VirtualFileSystem {
    root_fs: Mutex<Option<Arc<dyn FileSystem>>>,
}

impl VirtualFileSystem {
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

        let root_fs = self.root_fs.lock();
        let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;
        let root_inode = fs.root_inode()?;
        drop(root_fs);

        // 1. 从根 inode 逐层查找，使 `/missing/..` 不会被词法化简错误变成 `/`。
        // 2. inode 栈保留已验证的父链，`..` 只能回退到根，不会逃出当前文件系统。
        // 3. 最后校验尾随斜杠，避免把普通文件当目录打开。
        let mut inode_stack = Vec::from([root_inode]);
        for component in path.split(|byte| *byte == b'/') {
            match component {
                b"" | b"." => {}
                b".." => {
                    if inode_stack.len() > 1 {
                        inode_stack.pop();
                    }
                }
                name => {
                    let inode = inode_stack
                        .last()
                        .ok_or(FileSystemError::InvalidPath)?
                        .find_child(name)?;
                    if inode.inode_type() == InodeType::SymLink {
                        return Err(FileSystemError::InvalidOperation);
                    }
                    inode_stack.push(inode);
                }
            }
        }
        let inode = inode_stack.pop().ok_or(FileSystemError::InvalidPath)?;

        if path.len() > 1
            && path.last() == Some(&b'/')
            && inode.inode_type() != InodeType::Directory
        {
            return Err(FileSystemError::NotDirectory);
        }
        Ok(inode)
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
