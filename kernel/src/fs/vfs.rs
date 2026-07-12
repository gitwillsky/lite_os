use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{FileSystem, FileSystemError, FileSystemStatistics, Inode, InodeType};

/// @description 管理唯一 root namespace、boot mounts 与 pathname traversal。
pub(crate) struct VirtualFileSystem {
    root_fs: Mutex<Option<RootMount>>,
    mounts: Mutex<Vec<Mount>>,
}

struct RootMount {
    source: &'static [u8],
    filesystem: Arc<dyn FileSystem>,
    root: Arc<dyn Inode>,
}

struct Mount {
    source: &'static [u8],
    filesystem: Arc<dyn FileSystem>,
    point_identity: (usize, u64),
    root_identity: (usize, u64),
    point: Arc<dyn Inode>,
    parent: Arc<dyn Inode>,
    root: Arc<dyn Inode>,
}

impl VirtualFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self
            .root_fs
            .lock()
            .as_ref()
            .ok_or(FileSystemError::NotFound)?
            .root
            .clone())
    }

    fn identity(inode: &Arc<dyn Inode>) -> Result<(usize, u64), FileSystemError> {
        Ok((inode.filesystem_id(), inode.metadata()?.inode))
    }

    fn enter_mount(&self, inode: Arc<dyn Inode>) -> Result<Arc<dyn Inode>, FileSystemError> {
        let identity = Self::identity(&inode)?;
        Ok(self
            .mounts
            .lock()
            .iter()
            .find(|mount| mount.point_identity == identity)
            .map_or(inode, |mount| mount.root.clone()))
    }

    fn leave_mount(&self, inode: &Arc<dyn Inode>) -> Option<Arc<dyn Inode>> {
        let identity = Self::identity(inode).ok()?;
        self.mounts
            .lock()
            .iter()
            .find(|mount| mount.root_identity == identity)
            .map(|mount| mount.parent.clone())
    }

    fn mount_point(&self, inode: &Arc<dyn Inode>) -> Option<Arc<dyn Inode>> {
        let identity = Self::identity(inode).ok()?;
        self.mounts
            .lock()
            .iter()
            .find(|mount| mount.root_identity == identity)
            .map(|mount| mount.point.clone())
    }

    fn resolve_from(
        &self,
        start: Arc<dyn Inode>,
        path: &[u8],
        allow_final_symlink: bool,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.resolve_from_with_limit(start, path, allow_final_symlink, 0)
    }

    fn resolve_from_with_limit(
        &self,
        start: Arc<dyn Inode>,
        path: &[u8],
        allow_final_symlink: bool,
        followed_links: usize,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        const MAX_SYMLINKS: usize = 40;
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
                    if let Some(parent) = self.leave_mount(&inode) {
                        inode = parent;
                    } else if (inode.filesystem_id(), inode.metadata()?.inode) != root_identity {
                        inode = inode.find_child(b"..")?;
                    }
                }
                name => {
                    let parent = inode.clone();
                    inode = self.enter_mount(inode.find_child(name)?)?;
                    let is_untrailed_final = index + 1 == component_count
                        && path.last().is_none_or(|byte| *byte != b'/');
                    if inode.inode_type() == InodeType::SymLink
                        && !(allow_final_symlink && is_untrailed_final)
                    {
                        if followed_links >= MAX_SYMLINKS {
                            return Err(FileSystemError::SymbolicLink);
                        }
                        let target = inode.read_link()?;
                        if target.is_empty() {
                            return Err(FileSystemError::NotFound);
                        }
                        let remaining = path
                            .split(|byte| *byte == b'/')
                            .filter(|part| !matches!(*part, b"" | b"."))
                            .skip(index + 1);
                        let mut expanded = target;
                        for part in remaining {
                            if expanded.last() != Some(&b'/') {
                                expanded.push(b'/');
                            }
                            expanded.extend_from_slice(part);
                        }
                        if path.last() == Some(&b'/') && expanded.last() != Some(&b'/') {
                            expanded.push(b'/');
                        }
                        return self.resolve_from_with_limit(
                            parent,
                            &expanded,
                            allow_final_symlink,
                            followed_links + 1,
                        );
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
            mounts: Mutex::new(Vec::new()),
        }
    }

    /// 挂载唯一的根文件系统。
    ///
    /// # Parameters
    ///
    /// - `source`: `/proc/mounts` 中的 root source label。
    /// - `fs`: 根文件系统实例。
    ///
    /// # Returns
    ///
    /// 首次挂载成功时返回 `()`。
    ///
    /// # Errors
    ///
    /// 根文件系统已挂载时返回 `AlreadyExists`，防止静默替换启动卷。
    pub(crate) fn mount_root(
        &self,
        source: &'static [u8],
        fs: Arc<dyn FileSystem>,
    ) -> Result<(), FileSystemError> {
        let mut root_fs = self.root_fs.lock();
        if root_fs.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        let root = fs.root_inode()?;
        *root_fs = Some(RootMount {
            source,
            filesystem: fs,
            root,
        });
        Ok(())
    }

    /// @description 将一个 filesystem adapter 挂到已存在的 root-namespace 目录。
    ///
    /// @param path absolute mountpoint pathname；必须解析为尚未挂载的目录。
    /// @param source `/proc/mounts` 中的 mount source label。
    /// @param filesystem mount 后由 root inode owner 保活的 filesystem adapter。
    /// @return mount publication 完成时成功。
    /// @errors 路径、类型、重复 mount、adapter root 或内存分配失败时返回明确错误。
    pub(crate) fn mount_at(
        &self,
        path: &[u8],
        source: &'static [u8],
        filesystem: Arc<dyn FileSystem>,
    ) -> Result<(), FileSystemError> {
        if path.first() != Some(&b'/') {
            return Err(FileSystemError::InvalidPath);
        }
        let point = self.open(path)?;
        if point.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let root = filesystem.root_inode()?;
        if root.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let parent = point.find_child(b"..")?;
        let point_identity = Self::identity(&point)?;
        let root_identity = Self::identity(&root)?;
        let mut mounts = self.mounts.lock();
        if mounts.iter().any(|mount| {
            mount.point_identity == point_identity || mount.root_identity == root_identity
        }) {
            return Err(FileSystemError::AlreadyExists);
        }
        mounts
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        mounts.push(Mount {
            source,
            filesystem,
            point_identity,
            root_identity,
            point,
            parent,
            root,
        });
        Ok(())
    }

    /// @description 取得 inode 所属 mounted filesystem 的最终 Linux statfs 快照。
    ///
    /// @param inode pathname 或 OFD 已解析出的 inode。
    /// @return adapter 统计加当前 VFS mount flags。
    /// @errors inode 不属于当前 namespace 中的 mounted filesystem 时返回 `InvalidFileSystem`。
    pub(crate) fn statistics(
        &self,
        inode: Arc<dyn Inode>,
    ) -> Result<FileSystemStatistics, FileSystemError> {
        let filesystem_id = inode.filesystem_id();
        let root_filesystem = self.root_fs.lock().as_ref().and_then(|mount| {
            (mount.root.filesystem_id() == filesystem_id).then(|| mount.filesystem.clone())
        });
        let filesystem = root_filesystem.or_else(|| {
            self.mounts
                .lock()
                .iter()
                .find(|mount| mount.root_identity.0 == filesystem_id)
                .map(|mount| mount.filesystem.clone())
        });
        let mut statistics = filesystem
            .ok_or(FileSystemError::InvalidFileSystem)?
            .statistics();
        statistics.flags |= 0x20;
        Ok(statistics)
    }

    /// @description 将当前 root namespace 投影为 Linux `/proc/mounts` 文本。
    ///
    /// @return root 与所有 boot mounts 的 escaped mntent records。
    /// @errors mountpoint 反向解析失败或内存不足时返回明确文件系统错误。
    pub(crate) fn mount_table(&self) -> Result<Vec<u8>, FileSystemError> {
        let root = self
            .root_fs
            .lock()
            .as_ref()
            .map(|mount| (mount.source, mount.filesystem.clone()))
            .ok_or(FileSystemError::NotFound)?;
        let mounts = {
            let mounted = self.mounts.lock();
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(mounted.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            snapshot.extend(
                mounted
                    .iter()
                    .map(|mount| (mount.source, mount.point.clone(), mount.filesystem.clone())),
            );
            snapshot
        };
        let mut output = Vec::new();
        write_mount_record(&mut output, root.0, b"/", &root.1.statistics())?;
        for (source, point, filesystem) in mounts {
            let target = self.absolute_path(point)?;
            write_mount_record(&mut output, source, &target, &filesystem.statistics())?;
        }
        Ok(output)
    }

    /// @description 将 persistent root filesystem 的已提交写入同步到 block device stable storage。
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
    /// 路径非绝对路径、根文件系统未挂载、分量不存在、symlink loop，
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
    /// @param path raw pathname；中间 symbolic link 正常跟随，只保留未尾随的最终 link。
    /// @return 普通路径返回目标 inode，末项 symbolic link 返回 link inode 本身。
    /// @errors 路径不存在、symlink loop 或底层文件系统失败时返回错误。
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
            if let Some(point) = self.mount_point(&current) {
                current = point;
            }
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

    /// @description 在 new path 创建 raw-target symbolic link。
    /// @param start 相对 new path 的起始目录；None 表示 root。
    /// @param path 新链接 pathname。
    /// @param target 不经解析的 symbolic-link target bytes。
    /// @return 新 symbolic-link inode。
    /// @errors pathname、重复名称、空间、只读或底层 I/O 错误。
    pub(crate) fn symlink_at(
        &self,
        start: Option<Arc<dyn Inode>>,
        path: &[u8],
        target: &[u8],
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(start, path)?;
        parent.symlink(&name, target)
    }

    /// @description 为已解析目标创建同 filesystem 的硬链接目录项。
    /// @param target 不得为目录，且 final symlink 是否跟随已由 syscall/VFS caller 决定。
    /// @param new_start 相对 new path 的起始目录；None 表示 root。
    /// @param new_path 新硬链接 pathname。
    /// @return 成功或明确的跨 filesystem、类型及目录项错误。
    /// @errors 目标与 parent 分属不同 filesystem 时返回 CrossDevice。
    pub(crate) fn link_at(
        &self,
        target: Arc<dyn Inode>,
        new_start: Option<Arc<dyn Inode>>,
        new_path: &[u8],
    ) -> Result<(), FileSystemError> {
        let new_start = new_start.unwrap_or(self.root_inode()?);
        let (parent, name) = self.parent_from(new_start, new_path)?;
        if parent.filesystem_id() != target.filesystem_id() {
            return Err(FileSystemError::CrossDevice);
        }
        parent.link(&name, target)
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
            return Err(FileSystemError::CrossDevice);
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

fn write_mount_record(
    output: &mut Vec<u8>,
    source: &[u8],
    target: &[u8],
    statistics: &FileSystemStatistics,
) -> Result<(), FileSystemError> {
    let escaped_fields = source
        .len()
        .checked_add(target.len())
        .and_then(|length| length.checked_mul(4))
        .ok_or(FileSystemError::OutOfMemory)?;
    let required = escaped_fields
        .checked_add(statistics.type_name.len())
        .and_then(|length| length.checked_add(16))
        .ok_or(FileSystemError::OutOfMemory)?;
    output
        .try_reserve(required)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    write_mount_field(output, source);
    output.push(b' ');
    write_mount_field(output, target);
    output.push(b' ');
    output.extend_from_slice(statistics.type_name.as_bytes());
    output.extend_from_slice(if statistics.flags & 1 != 0 {
        b" ro 0 0\n"
    } else {
        b" rw 0 0\n"
    });
    Ok(())
}

fn write_mount_field(output: &mut Vec<u8>, field: &[u8]) {
    for byte in field {
        match byte {
            b' ' => output.extend_from_slice(b"\\040"),
            b'\t' => output.extend_from_slice(b"\\011"),
            b'\n' => output.extend_from_slice(b"\\012"),
            b'\\' => output.extend_from_slice(b"\\134"),
            byte => output.push(*byte),
        }
    }
}
