use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::device::DeviceNumber;
use super::{AccessIdentity, FileSystem, FileSystemError, FileSystemStatistics, Inode, InodeType};
use crate::sync::TaskMutex;

#[path = "vfs/mount_flags.rs"]
mod mount_flags;
#[path = "vfs/mount_table.rs"]
mod mount_table;
#[path = "vfs/mutation.rs"]
mod mutation;
#[path = "vfs/opened.rs"]
mod opened;
#[path = "vfs/opened_index.rs"]
mod opened_index;
pub(crate) use mount_flags::MountFlags;
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

/// 管理唯一 root namespace、boot mounts 与 pathname traversal。
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
    // table 会复制锁，若放进 ext4 adapter，devfs 与其他 mounted inode 会形成第二套语义。
    advisory_locks: Mutex<Vec<advisory_lock::AdvisoryFileLock>>,
    // OWNER: VFS inode identity → process-owned POSIX byte-range locks；若归 fd/OFD 所有，dup、fork
    // 与任一 descriptor close 会产生错误的锁生命周期。
    record_locks: Mutex<Vec<record_lock::RecordLock>>,
    // 唯一反向 adapter 只投递 key，不保存 task 状态；缺失时最后 descriptor close 无法唤醒 waiter。
    advisory_lock_notifier: Mutex<Option<Arc<dyn AdvisoryLockNotifier>>>,
}

struct RootMount {
    source: Vec<u8>,
    filesystem: Arc<dyn FileSystem>,
    root: Arc<OpenedFile>,
    /// 承载根文件系统的块设备。
    device: Option<DeviceNumber>,
    attributes: MountAttributes,
}

/// 挂载属性与以写方式打开的 OFD 数；二者在同一把锁内变化，read-only remount 才能原子判忙。
#[derive(Clone, Copy, Default)]
struct MountAttributes {
    flags: MountFlags,
    // 以写方式打开的 OFD 数（Linux `mnt_writers`）：缺失时 `remount,ro` 之后仍有可写 fd 在改文件。
    writers: usize,
}

struct Mount {
    source: Vec<u8>,
    filesystem: Arc<dyn FileSystem>,
    /// 承载该文件系统的块设备；nodev 文件系统为 `None`。同一块设备只能挂载一次。
    device: Option<DeviceNumber>,
    point_identity: (usize, u64),
    root_identity: (usize, u64),
    point: Arc<OpenedFile>,
    parent: Arc<OpenedFile>,
    root: Arc<OpenedFile>,
    attributes: MountAttributes,
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
    /// - `source`: `/proc/mounts` 中的 root source（块设备路径）。
    /// - `fs`: 根文件系统实例。
    /// - `device`: 承载根文件系统的块设备号。
    ///
    /// # Errors
    ///
    /// 根文件系统已挂载时返回 `AlreadyExists`，防止静默替换启动卷；分配失败返回 `OutOfMemory`。
    pub(crate) fn mount_root(
        &self,
        source: &[u8],
        fs: Arc<dyn FileSystem>,
        device: Option<DeviceNumber>,
        flags: MountFlags,
    ) -> Result<(), FileSystemError> {
        let source = owned_bytes(source)?;
        let mut root_fs = self.root_fs.lock();
        if root_fs.is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        let root = self.opened.register(OpenedFile::root(fs.root_inode()?)?)?;
        *root_fs = Some(RootMount {
            source,
            filesystem: fs,
            root,
            device,
            attributes: MountAttributes { flags, writers: 0 },
        });
        Ok(())
    }

    /// `device` 是否已承载一个已挂载的文件系统。
    pub(crate) fn device_mounted(&self, device: DeviceNumber) -> bool {
        self.root_fs
            .lock()
            .as_ref()
            .is_some_and(|root| root.device == Some(device))
            || self
                .mounts
                .lock()
                .iter()
                .any(|mount| mount.device == Some(device))
    }

    /// 把一个 filesystem adapter 挂到已解析的目录（Linux `do_new_mount`）。
    ///
    /// # Parameters
    ///
    /// - `point`: 已解析的 mountpoint；必须是目录，且不是已有挂载的根或挂载点。
    /// - `source`: `/proc/mounts` 中的 mount source。
    /// - `filesystem`: mount 后由 root inode owner 保活的 filesystem adapter。
    /// - `device`: 承载该文件系统的块设备号；nodev 文件系统为 `None`。
    /// - `flags`: 挂载属性。
    ///
    /// # Errors
    ///
    /// mountpoint 不是目录返回 `NotDirectory`；mountpoint 已被占用（含堆叠到挂载根或 `/`）或块设备
    /// 已挂载返回 `Busy`；adapter root 读取或分配失败返回对应错误。
    pub(crate) fn mount(
        &self,
        point: Arc<OpenedFile>,
        source: &[u8],
        filesystem: Arc<dyn FileSystem>,
        device: Option<DeviceNumber>,
        flags: MountFlags,
    ) -> Result<(), FileSystemError> {
        if point.inode().inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        let root_inode = filesystem.root_inode()?;
        if root_inode.inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        // namespace 根没有 parent，`leave_mount` 无法返回；同一目录的堆叠挂载尚不支持。
        let parent = point.parent().ok_or(FileSystemError::Busy)?;
        let point_identity = Self::identity(&point.inode())?;
        let root_identity = Self::identity(&root_inode)?;
        let source = owned_bytes(source)?;
        let point_name = point.location_name()?;
        let root =
            self.opened
                .register(OpenedFile::child(root_inode, parent.clone(), &point_name)?)?;
        let root_device = self.root_fs.lock().as_ref().and_then(|root| root.device);
        let mut mounts = self.mounts.lock();
        if device.is_some() && root_device == device
            || mounts.iter().any(|mount| {
                mount.point_identity == point_identity
                    || mount.root_identity == point_identity
                    || mount.root_identity == root_identity
                    || device.is_some() && mount.device == device
            })
        {
            return Err(FileSystemError::Busy);
        }
        mounts
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        mounts.push(Mount {
            source,
            filesystem,
            device,
            point_identity,
            root_identity,
            point,
            parent,
            root,
            attributes: MountAttributes { flags, writers: 0 },
        });
        Ok(())
    }

    /// 摘下以 `root` 为根的挂载（Linux `do_umount`）。
    ///
    /// 1. `root` 必须是某个挂载的根；namespace 根返回 `Busy`，普通目录返回 `InvalidOperation`；
    /// 2. 有子挂载返回 `Busy`；
    /// 3. 挂载记录与调用者各持有一个 `root` 引用。打开文件、cwd、mmap 与任何更深的打开条目都
    ///    经 parent 链持有 `root`，引用数超过 2 即返回 `Busy`。检查与摘除在 mounts 锁内完成，
    ///    与 `enter_mount` 串行，因此不会有新访问在检查后进入。
    ///
    /// # Returns
    ///
    /// 被摘下的 filesystem 与承载它的块设备；调用方负责写回 page cache、调用 [`FileSystem::shutdown`]
    /// 并释放块设备的挂载占用。
    pub(crate) fn unmount(
        &self,
        root: &Arc<OpenedFile>,
    ) -> Result<(Arc<dyn FileSystem>, Option<DeviceNumber>), FileSystemError> {
        let identity = Self::identity(&root.inode())?;
        let namespace_root = self
            .root_fs
            .lock()
            .as_ref()
            .is_some_and(|mount| Arc::ptr_eq(&mount.root, root));
        if namespace_root {
            return Err(FileSystemError::Busy);
        }
        let mut mounts = self.mounts.lock();
        let index = mounts
            .iter()
            .position(|mount| mount.root_identity == identity)
            .ok_or(FileSystemError::InvalidOperation)?;
        if mounts
            .iter()
            .any(|mount| mount.point_identity.0 == identity.0)
            || Arc::strong_count(&mounts[index].root) > 2
        {
            return Err(FileSystemError::Busy);
        }
        let mount = mounts.remove(index);
        Ok((mount.filesystem, mount.device))
    }

    /// 在 `mounts`/`root_fs` 锁内对承载 `filesystem_id` 的挂载属性执行 `visit`。
    ///
    /// 没有挂载承载该文件系统（例如匿名 memfd）时返回 `None`。
    fn with_attributes<R>(
        &self,
        filesystem_id: usize,
        visit: impl FnOnce(&mut MountAttributes) -> R,
    ) -> Option<R> {
        if let Some(root) = self.root_fs.lock().as_mut()
            && root.root.inode().filesystem_id() == filesystem_id
        {
            return Some(visit(&mut root.attributes));
        }
        self.mounts
            .lock()
            .iter_mut()
            .find(|mount| mount.root_identity.0 == filesystem_id)
            .map(|mount| visit(&mut mount.attributes))
    }

    /// 承载 `filesystem_id` 的挂载属性；没有挂载时为空集合。
    pub(crate) fn mount_flags(&self, filesystem_id: usize) -> MountFlags {
        self.with_attributes(filesystem_id, |attributes| attributes.flags)
            .unwrap_or_default()
    }

    /// 在会修改该文件系统的 namespace 或 metadata 的操作前调用（Linux `mnt_want_write`）。
    ///
    /// # Errors
    ///
    /// 挂载为 read-only 返回 `ReadOnly`。
    pub(crate) fn require_writable(&self, filesystem_id: usize) -> Result<(), FileSystemError> {
        if self.mount_flags(filesystem_id).read_only() {
            return Err(FileSystemError::ReadOnly);
        }
        Ok(())
    }

    /// 登记一个以写方式打开的 OFD；与 `remount,ro` 在同一把锁内互斥。
    ///
    /// # Errors
    ///
    /// 挂载为 read-only 返回 `ReadOnly`。
    pub(crate) fn begin_write_open(&self, filesystem_id: usize) -> Result<(), FileSystemError> {
        self.with_attributes(filesystem_id, |attributes| {
            if attributes.flags.read_only() {
                return Err(FileSystemError::ReadOnly);
            }
            attributes.writers += 1;
            Ok(())
        })
        .unwrap_or(Ok(()))
    }

    /// 撤销一次 [`Self::begin_write_open`]；挂载已被卸载时为空操作。
    pub(crate) fn end_write_open(&self, filesystem_id: usize) {
        self.with_attributes(filesystem_id, |attributes| {
            attributes.writers = attributes
                .writers
                .checked_sub(1)
                .expect("write-open count released without acquire");
        });
    }

    /// 替换以 `root` 为根的挂载的属性（Linux `do_remount` 的 `MS_REMOUNT` 路径）。
    ///
    /// # Returns
    ///
    /// 该挂载的 filesystem 与旧属性；调用者在 filesystem 重配置失败时用 [`Self::restore_mount_flags`]
    /// 还原。
    ///
    /// # Errors
    ///
    /// `root` 不是挂载根返回 `InvalidOperation`；转为 read-only 时仍有可写打开的 OFD 返回 `Busy`。
    pub(crate) fn replace_mount_flags(
        &self,
        root: &Arc<OpenedFile>,
        flags: MountFlags,
    ) -> Result<(Arc<dyn FileSystem>, MountFlags), FileSystemError> {
        let identity = Self::identity(&root.inode())?;
        let swap = |attributes: &mut MountAttributes| {
            if flags.read_only() && !attributes.flags.read_only() && attributes.writers != 0 {
                return Err(FileSystemError::Busy);
            }
            Ok(core::mem::replace(&mut attributes.flags, flags))
        };
        if let Some(mount) = self.root_fs.lock().as_mut()
            && Arc::ptr_eq(&mount.root, root)
        {
            return Ok((mount.filesystem.clone(), swap(&mut mount.attributes)?));
        }
        let mut mounts = self.mounts.lock();
        let mount = mounts
            .iter_mut()
            .find(|mount| mount.root_identity == identity)
            .ok_or(FileSystemError::InvalidOperation)?;
        Ok((mount.filesystem.clone(), swap(&mut mount.attributes)?))
    }

    /// 还原 [`Self::replace_mount_flags`] 之前的属性。
    pub(crate) fn restore_mount_flags(&self, filesystem_id: usize, flags: MountFlags) {
        self.with_attributes(filesystem_id, |attributes| attributes.flags = flags);
    }

    /// 取得 inode 所属 mounted filesystem 的最终 Linux statfs 快照。
    ///
    /// # Parameters
    ///
    /// - `inode`: pathname 或 OFD 已解析出的 inode。
    ///
    /// # Returns
    ///
    /// adapter 统计加当前 VFS mount flags。
    ///
    /// # Errors
    ///
    /// inode 不属于当前 namespace 中的 mounted filesystem 时返回 `InvalidFileSystem`。
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
        statistics.flags |= 0x20 | self.mount_flags(filesystem_id).bits();
        Ok(statistics)
    }

    /// 将当前 root namespace 投影为 Linux `/proc/mounts` 文本。
    ///
    /// # Returns
    ///
    /// root 与所有 boot mounts 的 escaped mntent records。
    ///
    /// # Errors
    ///
    /// mountpoint 反向解析失败或内存不足时返回明确文件系统错误。
    pub(crate) fn mount_table(&self) -> Result<Vec<u8>, FileSystemError> {
        let root = {
            let root = self.root_fs.lock();
            let root = root.as_ref().ok_or(FileSystemError::NotFound)?;
            (
                owned_bytes(&root.source)?,
                root.filesystem.clone(),
                root.attributes.flags,
            )
        };
        let mounts = {
            let mounted = self.mounts.lock();
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(mounted.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            for mount in mounted.iter() {
                snapshot.push((
                    owned_bytes(&mount.source)?,
                    mount.point.clone(),
                    mount.filesystem.clone(),
                    mount.attributes.flags,
                ));
            }
            snapshot
        };
        let mut output = Vec::new();
        write_mount_record(&mut output, &root.0, b"/", &root.1.statistics()?, root.2)?;
        for (source, point, filesystem, flags) in mounts {
            let target = self.absolute_path(point)?;
            write_mount_record(
                &mut output,
                &source,
                &target,
                &filesystem.statistics()?,
                flags,
            )?;
        }
        Ok(output)
    }

    /// 把 page cache 与全部已挂载文件系统的已提交写入同步到 stable storage（Linux `sync(2)`）。
    ///
    /// # Errors
    ///
    /// 根文件系统未挂载、分配失败或任一文件系统 flush 失败时返回明确文件系统错误；其余文件系统
    /// 仍会被同步。
    pub(crate) fn sync(&self) -> Result<(), FileSystemError> {
        super::sync_all()?;
        let count = self.mounts.lock().len();
        let mut roots = Vec::new();
        roots
            .try_reserve_exact(count + 1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        roots.push(self.root_inode()?);
        // 先在锁外预留，再在锁内只做不分配的复制；并发 mount 使数量增加时多出的挂载留给下一次 sync。
        for mount in self.mounts.lock().iter().take(count) {
            roots.push(mount.root.inode());
        }
        let mut result = Ok(());
        for root in roots {
            if let Err(error) = root.sync_storage() {
                result = result.and(Err(error));
            }
        }
        result
    }

    /// 从 root namespace 打开并保留标准 opened-entry identity。
    ///
    /// # Parameters
    ///
    /// - `path`: 绝对 pathname。
    ///
    /// # Returns
    ///
    /// VFS-owned opened entry。
    ///
    /// # Errors
    ///
    /// pathname、权限或内存失败时返回明确错误。
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

    /// 相对 opened directory 解析 pathname 并保留最终目录项身份。
    ///
    /// # Parameters
    ///
    /// - `start`: 相对 lookup 起点；None 表示 root。
    /// - `path`: raw pathname。
    /// - `identity`: traversal credential snapshot。
    ///
    /// # Returns
    ///
    /// 最终 opened entry。
    ///
    /// # Errors
    ///
    /// traversal、symlink 或资源失败时返回明确错误。
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

    /// 解析 pathname 但不跟随最终 symbolic link，保留最终目录项身份（`UMOUNT_NOFOLLOW`）。
    ///
    /// # Errors
    ///
    /// traversal 或资源失败时返回明确错误。
    pub(crate) fn open_file_at_no_follow(
        &self,
        start: Option<Arc<OpenedFile>>,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let start = match start {
            Some(start) => start,
            None => self.root_opened()?,
        };
        self.resolve_from(start, path, true, identity)
    }

    /// 解析 pathname 但保留最后一个 symbolic-link inode，供 Linux lstat 使用。
    ///
    /// # Parameters
    ///
    /// - `start`: 相对路径的起始目录；None 表示 root。
    /// - `path`: raw pathname；中间 symbolic link 正常跟随，只保留未尾随的最终 link。
    ///
    /// # Returns
    ///
    /// 普通路径返回目标 inode，末项 symbolic link 返回 link inode 本身。
    ///
    /// # Errors
    ///
    /// 路径不存在、symlink loop 或底层文件系统失败时返回错误。
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

    /// 从目录 inode identity 反向解析当前 namespace 中的 raw absolute path。
    ///
    /// # Parameters
    ///
    /// - `inode`: 必须属于当前 root filesystem 且为目录。
    ///
    /// # Returns
    ///
    /// root 返回 `/`；其他目录返回当前目录项关系对应的 absolute path。
    ///
    /// # Errors
    ///
    /// inode 已不可达、目录关系损坏、跨 filesystem 或底层 I/O 失败时返回明确错误。
    pub(crate) fn absolute_path(
        &self,
        opened: Arc<OpenedFile>,
    ) -> Result<Vec<u8>, FileSystemError> {
        if opened.inode().inode_type() != InodeType::Directory {
            return Err(FileSystemError::NotDirectory);
        }
        opened.path(false)
    }

    /// 投影 procfs fd symlink 使用的 opened pathname。
    ///
    /// # Parameters
    ///
    /// - `opened`: live OFD/cwd opened entry。
    ///
    /// # Returns
    ///
    /// 当前路径；任一祖先已删除时追加 Linux ` (deleted)` 后缀。
    ///
    /// # Errors
    ///
    /// opened-entry 链损坏或内存不足时返回明确错误。
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

/// 复制一段 mount source 字节。
fn owned_bytes(bytes: &[u8]) -> Result<Vec<u8>, FileSystemError> {
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(bytes.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    owned.extend_from_slice(bytes);
    Ok(owned)
}
