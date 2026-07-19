use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{AccessIdentity, FileSystem, FileSystemError, FileSystemStatistics, Inode, InodeType};
use crate::sync::TaskMutex;

#[path = "vfs/mount_table.rs"]
mod mount_table;
#[path = "vfs/mutation.rs"]
mod mutation;
#[path = "vfs/opened.rs"]
mod opened;
#[path = "vfs/opened_index.rs"]
mod opened_index;
use mount_table::write_mount_record;
pub(crate) use opened::OpenedFile;
use opened_index::OpenedIndex;
#[path = "vfs/advisory_lock.rs"]
mod advisory_lock;
#[path = "vfs/record_lock.rs"]
mod record_lock;
pub(crate) use advisory_lock::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, PreparedAdvisoryLock, PreparedLockAttempt,
};
pub(crate) use record_lock::{PreparedRecordLock, RecordLockMode, RecordLockRange};

/// @description 管理唯一 root namespace、boot mounts 与 pathname traversal。
pub(crate) struct VirtualFileSystem {
    root_fs: Mutex<Option<RootMount>>,
    mounts: Mutex<Vec<Mount>>,
    // OWNER: VFS namespace mutation lock serializes adapter commit with opened-entry publication；
    // 缺失时并发 A→B→C rename 可让磁盘停在 C、registry 因乱序停在 B。
    namespace_mutation: TaskMutex<()>,
    // OWNER: VFS 的 exact opened index 唯一路由 register、rename/unlink 和 final Drop；
    // 缺失 exact lifecycle membership 会迫使每个路径组件扫描全部 live Weak entries。
    opened: OpenedIndex,
    // OWNER: VFS inode identity → OFD-owned BSD flock state；若放进 fd table，fork 后的独立
    // table 会复制锁，若放进 ext2 adapter，devfs 与其他 mounted inode 会形成第二套语义。
    advisory_locks: Mutex<Vec<advisory_lock::AdvisoryFileLock>>,
    // OWNER: VFS inode identity → process-owned POSIX byte-range locks；若归 fd/OFD 所有，dup、fork
    // 与任一 descriptor close 会产生错误的锁生命周期。
    record_locks: Mutex<Vec<record_lock::RecordLock>>,
    // 唯一反向 adapter 只投递 key，不保存 task 状态；缺失时最后 descriptor close 无法唤醒 waiter。
    advisory_lock_notifier: Mutex<Option<Arc<dyn AdvisoryLockNotifier>>>,
}

struct RootMount {
    source: &'static [u8],
    filesystem: Arc<dyn FileSystem>,
    root: Arc<OpenedFile>,
}

struct Mount {
    source: &'static [u8],
    filesystem: Arc<dyn FileSystem>,
    point_identity: (usize, u64),
    root_identity: (usize, u64),
    point: Arc<OpenedFile>,
    parent: Arc<OpenedFile>,
    root: Arc<OpenedFile>,
}

impl VirtualFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self
            .root_fs
            .lock()
            .as_ref()
            .ok_or(FileSystemError::NotFound)?
            .root
            .inode())
    }

    fn root_opened(&self) -> Result<Arc<OpenedFile>, FileSystemError> {
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

    fn enter_mount(&self, opened: Arc<OpenedFile>) -> Result<Arc<OpenedFile>, FileSystemError> {
        let identity = Self::identity(&opened.inode())?;
        Ok(self
            .mounts
            .lock()
            .iter()
            .find(|mount| mount.point_identity == identity)
            .map_or(opened, |mount| mount.root.clone()))
    }

    fn leave_mount(&self, opened: &Arc<OpenedFile>) -> Option<Arc<OpenedFile>> {
        let identity = Self::identity(&opened.inode()).ok()?;
        self.mounts
            .lock()
            .iter()
            .find(|mount| mount.root_identity == identity)
            .map(|mount| mount.parent.clone())
    }

    fn is_mount_point(&self, inode: &Arc<dyn Inode>) -> bool {
        let Ok(identity) = Self::identity(inode) else {
            return false;
        };
        self.mounts
            .lock()
            .iter()
            .any(|mount| mount.point_identity == identity || mount.root_identity == identity)
    }

    fn resolve_from(
        &self,
        start: Arc<OpenedFile>,
        path: &[u8],
        allow_final_symlink: bool,
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        self.resolve_from_with_limit(start, path, allow_final_symlink, identity, 0)
    }

    fn resolve_from_with_limit(
        &self,
        start: Arc<OpenedFile>,
        path: &[u8],
        allow_final_symlink: bool,
        identity: &AccessIdentity,
        followed_links: usize,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        const MAX_SYMLINKS: usize = 40;
        let root = self.root_opened()?;
        let mut opened = if path.first() == Some(&b'/') {
            root.clone()
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
            identity.require(opened.inode().metadata()?, 1)?;
            match component {
                b".." => {
                    if let Some(parent) = self.leave_mount(&opened) {
                        opened = parent;
                    } else if !opened.same_inode(&root) {
                        opened = opened.parent().ok_or(FileSystemError::InvalidFileSystem)?;
                    }
                }
                name => {
                    let parent = opened.clone();
                    let inode = parent.inode().find_child(name)?;
                    opened =
                        self.opened
                            .register(OpenedFile::child(inode, parent.clone(), name)?)?;
                    opened = self.enter_mount(opened)?;
                    let is_untrailed_final = index + 1 == component_count
                        && path.last().is_none_or(|byte| *byte != b'/');
                    if opened.inode().inode_type() == InodeType::SymLink
                        && !(allow_final_symlink && is_untrailed_final)
                    {
                        if followed_links >= MAX_SYMLINKS {
                            return Err(FileSystemError::SymbolicLink);
                        }
                        if let Some(target) = opened.inode().follow_link() {
                            let mut remaining = Vec::new();
                            remaining
                                .try_reserve_exact(path.len())
                                .map_err(|_| FileSystemError::OutOfMemory)?;
                            for part in path
                                .split(|byte| *byte == b'/')
                                .filter(|part| !matches!(*part, b"" | b"."))
                                .skip(index + 1)
                            {
                                if !remaining.is_empty() {
                                    remaining.push(b'/');
                                }
                                remaining.extend_from_slice(part);
                            }
                            if remaining.is_empty() {
                                if path.last() == Some(&b'/')
                                    && target.inode().inode_type() != InodeType::Directory
                                {
                                    return Err(FileSystemError::NotDirectory);
                                }
                                return Ok(target);
                            }
                            return self.resolve_from_with_limit(
                                target,
                                &remaining,
                                allow_final_symlink,
                                identity,
                                followed_links + 1,
                            );
                        }
                        let target = opened.inode().read_link()?;
                        if target.is_empty() {
                            return Err(FileSystemError::NotFound);
                        }
                        let remaining = path
                            .split(|byte| *byte == b'/')
                            .filter(|part| !matches!(*part, b"" | b"."))
                            .skip(index + 1);
                        let mut expanded = target;
                        // remaining path 是原 path 的子序列；一次预留 path.len()
                        // 覆盖所有分隔符与 trailing slash，缺失时 push 会走全局 OOM abort。
                        expanded
                            .try_reserve(path.len())
                            .map_err(|_| FileSystemError::OutOfMemory)?;
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
                            identity,
                            followed_links + 1,
                        );
                    }
                }
            }
        }
        if component_count == 0 && opened.inode().inode_type() == InodeType::Directory {
            identity.require(opened.inode().metadata()?, 1)?;
        }
        if path.len() > 1
            && path.last() == Some(&b'/')
            && opened.inode().inode_type() != InodeType::Directory
        {
            return Err(FileSystemError::NotDirectory);
        }
        Ok(opened)
    }

    fn parent_from(
        &self,
        start: Arc<OpenedFile>,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<(Arc<OpenedFile>, Vec<u8>), FileSystemError> {
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
        let mut owned_name = Vec::new();
        owned_name
            .try_reserve_exact(name.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        owned_name.extend_from_slice(name);
        Ok((
            self.resolve_from(start, parent_path, false, identity)?,
            owned_name,
        ))
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
            namespace_mutation: TaskMutex::new(()),
            opened: OpenedIndex::new(),
            advisory_locks: Mutex::new(Vec::new()),
            record_locks: Mutex::new(Vec::new()),
            advisory_lock_notifier: Mutex::new(None),
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
        let root = self.opened.register(OpenedFile::root(fs.root_inode()?)?)?;
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
        let point = self.open_file(path)?;
        if point.inode().inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let root_inode = filesystem.root_inode()?;
        if root_inode.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let parent = point.parent().ok_or(FileSystemError::InvalidPath)?;
        let point_identity = Self::identity(&point.inode())?;
        let root_identity = Self::identity(&root_inode)?;
        let point_name = point.location_name()?;
        let root =
            self.opened
                .register(OpenedFile::child(root_inode, parent.clone(), &point_name)?)?;
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
            (mount.root.inode().filesystem_id() == filesystem_id).then(|| mount.filesystem.clone())
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
            .statistics()?;
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
        write_mount_record(&mut output, root.0, b"/", &root.1.statistics()?)?;
        for (source, point, filesystem) in mounts {
            let target = self.absolute_path(point)?;
            write_mount_record(&mut output, source, &target, &filesystem.statistics()?)?;
        }
        Ok(output)
    }

    /// @description 将 persistent root filesystem 的已提交写入同步到 block device stable storage。
    ///
    /// @return flush 完成时成功。
    /// @errors 根文件系统未挂载或 block device flush 失败时返回明确文件系统错误。
    pub(crate) fn sync(&self) -> Result<(), FileSystemError> {
        super::sync_all()?;
        self.root_inode()?.sync_storage()
    }

    /// @description 从 root namespace 打开并保留标准 opened-entry identity。
    /// @param path 绝对 pathname。
    /// @return VFS-owned opened entry。
    /// @errors pathname、权限或内存失败时返回明确错误。
    pub(crate) fn open_file(&self, path: &[u8]) -> Result<Arc<OpenedFile>, FileSystemError> {
        if path.first() != Some(&b'/') {
            return Err(FileSystemError::InvalidPath);
        }
        self.resolve_from(self.root_opened()?, path, false, &AccessIdentity::root())
    }

    pub(crate) fn open_at(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.open_file_at(start, path, identity)
            .map(|opened| opened.inode())
    }

    /// @description 相对 opened directory 解析 pathname 并保留最终目录项身份。
    /// @param start 相对 lookup 起点；None 表示 root。
    /// @param path raw pathname。
    /// @param identity traversal credential snapshot。
    /// @return 最终 opened entry。
    /// @errors traversal、symlink 或资源失败时返回明确错误。
    pub(crate) fn open_file_at(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        self.resolve_from(start, path, false, identity)
    }

    /// @description 解析 pathname 但保留最后一个 symbolic-link inode，供 Linux lstat 使用。
    ///
    /// @param start 相对路径的起始目录；None 表示 root。
    /// @param path raw pathname；中间 symbolic link 正常跟随，只保留未尾随的最终 link。
    /// @return 普通路径返回目标 inode，末项 symbolic link 返回 link inode 本身。
    /// @errors 路径不存在、symlink loop 或底层文件系统失败时返回错误。
    pub(crate) fn open_at_no_follow(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        self.resolve_from(start, path, true, identity)
            .map(|opened| opened.inode())
    }

    /// @description 从目录 inode identity 反向解析当前 namespace 中的 raw absolute path。
    ///
    /// @param inode 必须属于当前 root filesystem 且为目录。
    /// @return root 返回 `/`；其他目录返回当前目录项关系对应的 absolute path。
    /// @errors inode 已不可达、目录关系损坏、跨 filesystem 或底层 I/O 失败时返回明确错误。
    pub(crate) fn absolute_path(
        &self,
        opened: Arc<OpenedFile>,
    ) -> Result<Vec<u8>, FileSystemError> {
        if opened.inode().inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        opened.path(false)
    }

    /// @description 投影 procfs fd symlink 使用的 opened pathname。
    /// @param opened live OFD/cwd opened entry。
    /// @return 当前路径；任一祖先已删除时追加 Linux ` (deleted)` 后缀。
    /// @errors opened-entry 链损坏或内存不足时返回明确错误。
    pub(crate) fn opened_path(&self, opened: &Arc<OpenedFile>) -> Result<Vec<u8>, FileSystemError> {
        opened.path(true)
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
