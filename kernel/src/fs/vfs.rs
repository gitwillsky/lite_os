use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use super::{AccessIdentity, FileSystemError, FileSystemStatistics, Inode, InodeType};
use crate::sync::TaskMutex;

mod lookup_origin;
mod mount_flags;
mod mount_operations;
mod mount_table;
mod mutation;
mod opened;
mod opened_xattr;
mod path_lookup;
pub(crate) use lookup_origin::LookupOrigin;
mod dentry;
mod dentry_index;
mod mount_attachment;
mod mount_copy;
mod mount_index;
mod mount_limit;
pub(crate) use mount_limit::{mount_limit, set_mount_limit};
mod mount_namespace;
mod mount_propagation;
mod mount_publication;
mod mount_retirement;
mod mount_unmount;
mod mount_view;
mod mounted_filesystem;
mod namespace_access;
pub(in crate::fs) use dentry::Dentry;
use dentry_index::DentryIndex;
pub(crate) use mount_flags::MountFlags;
pub(crate) use mount_namespace::{MountNamespace, UnmountMode};
pub(crate) use mount_propagation::MountPropagation;
pub(crate) use mount_table::MountTableKind;
use mount_table::write_mount_record;
pub(crate) use mount_view::WriteOpen;
use mount_view::{Attachment, MountRestrictions, MountScope, MountView};
pub(in crate::fs) use mounted_filesystem::{MountDeviceClaim, MountedFilesystem};
pub(crate) use opened::{ContentWrite, OpenedFile, ReadAccess};
use spin::Once;
/// pseudo filesystem 唯一internal mount producer；不加入任何namespace森林或quota/count/index。
/// scope ID 0只用于未加入namespace的内部mount，live namespace identity从1开始；空root不pin namespace。
/// 与用户mount共用真实view/FS/writer/retirement owner，失败按同一RAII释放，不发表另一份flags。
pub(in crate::fs) fn internal_mount(
    filesystem: Arc<MountedFilesystem>,
    root_inode: Arc<dyn Inode>,
    source: Arc<Vec<u8>>,
) -> Result<Arc<OpenedFile>, FileSystemError> {
    let scope = MountScope::new(0, filesystem.user.clone())?;
    let root = Dentry::named(root_inode, None, b"")?;
    let view = MountView::new(
        scope,
        source,
        filesystem,
        root.clone(),
        None,
        MountFlags::from_bits(0),
        MountRestrictions::default(),
    )?;
    OpenedFile::at(view, root)
}
mod advisory_lock;
mod record_lock;
pub(crate) use advisory_lock::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, LockHolder, PreparedAdvisoryLock, PreparedLockAttempt,
};
pub(crate) use record_lock::{
    PreparedRecordLock, RecordLockMode, RecordLockOwner, RecordLockRange,
};

/// Canonical filesystem dentries、实际 attachment traversal 与全局路径 mutation transaction。
/// namespace 各自拥有 topology；这里只保留 boot initial namespace，不按当前 Task 隐式选路径。
pub(crate) struct VirtualFileSystem {
    initial: Arc<MountNamespace>,
    // OWNER: mount 拓扑、路径投影与 dentry 位置发布的短事务；不跨 filesystem I/O 持有。
    namespace_mutation: TaskMutex<()>,
    dentries: DentryIndex,
    advisory_locks: Mutex<Vec<advisory_lock::AdvisoryFileLock>>,
    record_locks: Mutex<Vec<record_lock::RecordLock>>,
    advisory_lock_notifier: Mutex<Option<Arc<dyn AdvisoryLockNotifier>>>,
}
impl VirtualFileSystem {
    fn try_new() -> Result<Self, FileSystemError> {
        // init namespace 是 worker 的实际 membership owner，必须早于任何 kernel thread；
        // root forest 在 mount_root 后一次安装，不换掉这些已捕获的 namespace handles。
        let scope = MountScope::new(1, crate::namespace::initial_user())?;
        Ok(Self {
            initial: MountNamespace::initial(scope)?,
            namespace_mutation: TaskMutex::new(()),
            dentries: DentryIndex::new(),
            advisory_locks: Mutex::new(Vec::new()),
            record_locks: Mutex::new(Vec::new()),
            advisory_lock_notifier: Mutex::new(None),
        })
    }
    pub(crate) fn initial_namespace(&self) -> Arc<MountNamespace> {
        self.initial.clone()
    }
    /// setns 的 LOOKUP_DOWN root：准备实际最上层 root，再交给 caller 原子安装 FS/namespace。
    pub(crate) fn namespace_root(
        &self,
        target: &Arc<MountNamespace>,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        let transaction = self.namespace()?;
        transaction.enter_mount(target.root()?)
    }
    fn root_opened(&self) -> Result<Arc<OpenedFile>, FileSystemError> {
        self.initial.root()
    }
    pub(in crate::fs) fn mount_root(
        &self,
        filesystem: Arc<MountedFilesystem>,
        source: Arc<Vec<u8>>,
        root_inode: Arc<dyn Inode>,
        flags: MountFlags,
    ) -> Result<(), FileSystemError> {
        let _transaction = self.namespace()?;
        let dentry = self.dentries.admit(Dentry::named(root_inode, None, b"")?)?;
        let scope = self.initial.scope.clone();
        let view = MountView::new(
            scope.clone(),
            source,
            filesystem.clone(),
            dentry,
            None,
            flags,
            MountRestrictions::default(),
        )?;
        self.initial.install_root(view)
    }
    pub(in crate::fs) fn unmount(
        &self,
        namespace: &Arc<MountNamespace>,
        root: &Arc<OpenedFile>,
        mode: UnmountMode,
    ) -> Result<Vec<Arc<crate::sync::TaskEvent>>, FileSystemError> {
        let _transaction = self.namespace()?;
        namespace.remove(root, mode)
    }
    /// 唯一 remount transaction：准入等待、准备与最终属性发表都属于实际 view/superblock owner。
    pub(in crate::fs) fn remount(
        &self,
        namespace: &Arc<MountNamespace>,
        identity: &AccessIdentity,
        root: &Arc<OpenedFile>,
        flags: MountFlags,
        options: &[u8],
        bind: bool,
    ) -> Result<(), FileSystemError> {
        let view = {
            let _transaction = self.namespace()?;
            let view = root
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?;
            if !namespace.contains(view) || !Arc::ptr_eq(&root.dentry, &view.root) {
                return Err(FileSystemError::InvalidOperation);
            }
            view.clone()
        };
        // 实际 path pin 阻止普通卸载；lazy detach 可撤销 membership，但不撤销已开始的
        // superblock 配置。admission 唯一串行配置与 writer，等待后端期间不能持拓扑锁。
        view.reconfigure(
            if bind {
                mount_view::Reconfiguration::View(flags)
            } else {
                mount_view::Reconfiguration::MountAndSuperblock(flags)
            },
            options,
            identity,
        )?;
        namespace.scope.changed();
        Ok(())
    }
    /// do_umount_root：只把实际 superblock 设为 RO，不摘 caller 的 root mount 或重写 view flags。
    pub(in crate::fs) fn unmount_root_read_only(
        &self,
        namespace: &Arc<MountNamespace>,
        root: &Arc<OpenedFile>,
        identity: &AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let view = {
            let _transaction = self.namespace()?;
            namespace.unmount_view(root)?.clone()
        };
        view.reconfigure(
            mount_view::Reconfiguration::SuperblockReadOnly,
            &[],
            identity,
        )?;
        view.scope.changed();
        Ok(())
    }
    pub(crate) fn statistics(
        &self,
        path: &Arc<OpenedFile>,
    ) -> Result<FileSystemStatistics, FileSystemError> {
        if let Some(statistics) = path.inode().pseudo_statistics() {
            return Ok(statistics);
        }
        let view = path
            .mount
            .as_ref()
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let filesystem = &view.filesystem.filesystem;
        let mut statistics = filesystem.statistics()?;
        statistics.flags |=
            0x20 | view.flags().bits() | u64::from(filesystem.superblock().read_only);
        Ok(statistics)
    }
    /// proc opened snapshot 提供被观察的实际 namespace 和该目标 FS root；不读取全局 mount table。
    pub(crate) fn mount_table(
        &self,
        target: &Arc<MountNamespace>,
        root: &Arc<OpenedFile>,
        kind: MountTableKind,
    ) -> Result<Vec<u8>, FileSystemError> {
        let _transaction = self.namespace()?;
        let views = target.snapshot()?;
        let mut output = Vec::new();
        for view in views {
            let path = OpenedFile::at(view.clone(), view.root.clone())?;
            let (destination, reachable) = path.project_path(root, false)?;
            if !reachable {
                continue;
            }
            // bind subtree 的 mount root 是实际 filesystem dentry 路径，与 namespace mountpoint 区分。
            let filesystem_root = self.dentries.admit(Dentry::named(
                view.filesystem.filesystem.root_inode()?,
                None,
                b"",
            )?)?;
            let relative = path.filesystem_path(&filesystem_root)?;
            write_mount_record(&mut output, &view, root, &destination, &relative, kind)?;
        }
        Ok(output)
    }
    pub(crate) fn sync(&self) -> Result<(), FileSystemError> {
        super::sync_all()?;
        mounted_filesystem::sync_all()
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
    pub(crate) fn open_swap_at(
        &self,
        origin: LookupOrigin,
        path: &[u8],
        identity: &AccessIdentity,
    ) -> Result<super::swap::SwapSource, FileSystemError> {
        let opened = self.open_file_at(origin, path, identity)?;
        let inode = opened.inode();
        let metadata = inode.metadata()?;
        identity.require(metadata, 6)?;
        let write_open = match inode.inode_type() {
            InodeType::File => Some(opened.begin_write()?),
            InodeType::BlockDevice => {
                if opened.mount_flags().nodev() {
                    return Err(FileSystemError::AccessDenied);
                }
                None
            }
            _ => return Err(FileSystemError::InvalidOperation),
        };
        Ok(super::swap::SwapSource {
            opened,
            _write_open: write_open,
        })
    }

    pub(in crate::fs) fn pin_swap_file(
        &self,
        opened: &Arc<OpenedFile>,
    ) -> Result<alloc::boxed::Box<dyn super::swap::SwapFileLease>, FileSystemError> {
        let _namespace = self.namespace()?;
        if opened.is_unlinked() {
            return Err(FileSystemError::NotFound);
        }
        opened.inode().pin_swap_file()
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
    pub(crate) fn opened_path(
        &self,
        opened: &Arc<OpenedFile>,
        root: &Arc<OpenedFile>,
    ) -> Result<Vec<u8>, FileSystemError> {
        self.namespace()?.path(opened, root, true)
    }
}

// OWNER: VFS singleton owns canonical dentries and boot namespace, never a Task membership mirror.
pub(crate) static VFS_MANAGER: Once<VirtualFileSystem> = Once::new();
pub(crate) fn init() {
    VFS_MANAGER
        .try_call_once(VirtualFileSystem::try_new)
        .expect("initial mount namespace allocation failed");
}
pub(crate) fn vfs() -> &'static VirtualFileSystem {
    VFS_MANAGER.wait()
}
