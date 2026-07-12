use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{FileSystem, FileSystemError, Inode, InodeType};

/// @description 管理唯一根文件系统，并为内核 ELF 加载器解析绝对路径。
pub(crate) struct VirtualFileSystem {
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
        allow_final_symlink: bool,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let root = self.root_inode()?;
        let root_identity = (root.filesystem_id(), root.metadata()?.inode);
        let mut inode = if path.first() == Some(&b'/') {
            root
        } else {
            start
        };
        let component_count = path
            .split(|byte| *byte == b'/')
            .filter(|component| !matches!(*component, b"" | b"."))
            .count();
        for (index, component) in path
            .split(|byte| *byte == b'/')
            .filter(|component| !matches!(*component, b"" | b"."))
            .enumerate()
        {
            match component {
                b".." => {
                    if (inode.filesystem_id(), inode.metadata()?.inode) != root_identity {
                        inode = inode.find_child(b"..")?;
                    }
                }
                name => {
                    inode = inode.find_child(name)?;
                    let is_untrailed_final = index + 1 == component_count
                        && path.last().is_none_or(|byte| *byte != b'/');
                    if inode.inode_type() == InodeType::SymLink
                        && !(allow_final_symlink && is_untrailed_final)
                    {
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
        Ok((self.resolve_from(start, parent_path, false)?, name.to_vec()))
    }

    /// 创建尚未挂载根文件系统的 VFS。
    ///
    /// # Returns
    ///
    /// 空的 VFS 实例。
    pub(crate) fn new() -> Self {
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
    pub(crate) fn mount_root(&self, fs: Arc<dyn FileSystem>) -> Result<(), FileSystemError> {
        let mut root_fs = self.root_fs.lock();
        if root_fs.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        *root_fs = Some(fs);
        Ok(())
    }

    /// @description 将唯一根文件系统的已提交写入同步到 block device stable storage。
    ///
    /// @return flush 完成时成功。
    /// @errors 根文件系统未挂载或 block device flush 失败时返回明确文件系统错误。
    pub(crate) fn sync(&self) -> Result<(), FileSystemError> {
        self.root_inode()?.sync()
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
    pub(crate) fn open(&self, path: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        if path.first() != Some(&b'/') {
            return Err(FileSystemError::InvalidPath);
        }
        self.resolve_from(self.root_inode()?, path, false)
    }

    pub(crate) fn open_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        self.resolve_from(start, path, false)
    }

    /// @description 解析 pathname 但保留最后一个 symbolic-link inode，供 Linux lstat 使用。
    ///
    /// @param start 相对路径的起始目录；None 表示 root。
    /// @param path raw pathname；中间 symbolic link 仍因当前无 symlink traversal 返回错误。
    /// @return 普通路径返回目标 inode，末项 symbolic link 返回 link inode 本身。
    /// @errors 路径不存在、越过不支持的中间 symbolic link 或底层文件系统失败时返回错误。
    pub(crate) fn open_at_no_follow(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        self.resolve_from(start, path, true)
    }

    /// @description 从目录 inode identity 反向解析当前 namespace 中的 raw absolute path。
    ///
    /// @param inode 必须属于当前 root filesystem 且为目录。
    /// @return root 返回 `/`；其他目录返回当前目录项关系对应的 absolute path。
    /// @errors inode 已不可达、目录关系损坏、跨 filesystem 或底层 I/O 失败时返回明确错误。
    pub(crate) fn absolute_path(&self, inode: Arc<dyn Inode>) -> Result<Vec<u8>, FileSystemError> {
        if inode.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let root = self.root_inode()?;
        let root_identity = (root.filesystem_id(), root.metadata()?.inode);
        let mut current = inode;
        let mut components = Vec::new();
        let mut visited = Vec::new();
        loop {
            let identity = (current.filesystem_id(), current.metadata()?.inode);
            if identity == root_identity {
                break;
            }
            if current.filesystem_id() != root.filesystem_id() || visited.contains(&identity) {
                return Err(FileSystemError::InvalidFileSystem);
            }
            visited.push(identity);
            let parent = current.find_child(b"..")?;
            let name = parent
                .list()?
                .into_iter()
                .find(|entry| {
                    entry.inode == identity.1
                        && entry.name.as_slice() != b"."
                        && entry.name.as_slice() != b".."
                })
                .ok_or(FileSystemError::NotFound)?
                .name;
            components.push(name);
            current = parent;
        }

        let names_size = components
            .iter()
            .try_fold(0usize, |size, component| size.checked_add(component.len()));
        let size = names_size
            .and_then(|size| size.checked_add(components.len().saturating_sub(1)))
            .and_then(|size| size.checked_add(1))
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let mut path = Vec::new();
        path.try_reserve_exact(size)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        path.push(b'/');
        for component in components.iter().rev() {
            if path.len() > 1 {
                path.push(b'/');
            }
            path.extend_from_slice(component);
        }
        Ok(path)
    }

    pub(crate) fn create_at(
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

    pub(crate) fn unlink_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        directory: bool,
    ) -> Result<(), FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path)?;
        parent.unlink(&name, directory)
    }

    pub(crate) fn rename_at(
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

// OWNER: VFS module owns the unique namespace and root mount table.
pub(crate) static VFS_MANAGER: Once<VirtualFileSystem> = Once::new();

pub(crate) fn init() {
    VFS_MANAGER.call_once(VirtualFileSystem::new);
}

pub(crate) fn vfs() -> &'static VirtualFileSystem {
    VFS_MANAGER.wait()
}
