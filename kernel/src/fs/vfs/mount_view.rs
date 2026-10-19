//! 一个 namespace 中的实际 mount view；路径 pin 此 owner，不按 filesystem ID 猜挂载。
use super::mount_index::AttachmentKey;
use super::{FileSystemError, MountFlags, dentry::Dentry, mounted_filesystem::MountedFilesystem};
use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

// OWNER: 已分配 mount identity 永不复用；namespace copy 创建新 view，bind 也不能沿用 source ID。
static NEXT_MOUNT_ID: AtomicU64 = AtomicU64::new(1);

/// Namespace 的独立 scope；identity/user immutable，event/通知在此拥有；view 不保活 topology/quota。
pub(super) struct MountScope {
    pub(super) id: u64,
    pub(super) user: Arc<crate::namespace::UserNamespace>,
    // OWNER: topology event 与 poll 唤醒属于此 namespace scope；旧 FD 只持实际 scope，不制造反向环。
    event: AtomicU64,
    // OWNER: live namespace membership的resource accounting；index admission/退休各只改一次。
    // 不复制forest节点，move只换registration位置，不能增减namespace mount count。
    mounts: AtomicUsize,
    notification: (Arc<crate::ipc::PipeEnd>, Arc<crate::ipc::PipeEnd>),
}
impl MountScope {
    pub(super) fn new(
        id: u64,
        user: Arc<crate::namespace::UserNamespace>,
    ) -> Result<Arc<Self>, FileSystemError> {
        let notification =
            crate::ipc::Pipe::notification_pair().map_err(|_| FileSystemError::OutOfMemory)?;
        Arc::try_new(Self {
            id,
            user,
            event: AtomicU64::new(crate::sync::next_readiness_generation()),
            mounts: AtomicUsize::new(0),
            notification,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn changed(&self) {
        self.event
            .store(crate::sync::next_readiness_generation(), Ordering::Release);
        self.notification.1.signal_readiness();
    }
    pub(super) fn prepare_additions(&self, additional: usize) -> Result<(), FileSystemError> {
        let current = self.mounts.load(Ordering::Relaxed);
        // copy_mnt_ns建立新namespace时直接继承整树；count_mounts只限制已有namespace的attach。
        if additional != 0
            && current != 0
            && current
                .checked_add(additional)
                .is_none_or(|count| count > super::mount_limit() as usize)
        {
            return Err(FileSystemError::NoSpace);
        }
        Ok(())
    }
    pub(super) fn admit_mount(&self) {
        self.mounts.fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn retire_mount(&self) {
        let previous = self.mounts.fetch_sub(1, Ordering::Relaxed);
        assert!(previous != 0, "mount namespace accounting underflow");
    }
    pub(super) fn event(&self) -> u64 {
        self.event.load(Ordering::Acquire)
    }
    pub(super) fn notification_pipe(&self) -> Arc<crate::ipc::Pipe> {
        self.notification.0.pipe()
    }
    pub(super) fn drain_notification(&self) {
        self.notification.0.drain_readiness();
    }
}
#[derive(Clone)]
pub(super) struct Attachment {
    pub(super) parent: Weak<MountView>,
    pub(super) point: Arc<Dentry>,
    // dput mountpoint 必须先于所属 physical filesystem 的最后 release；weak parent 不承担此租约。
    _parent_filesystem: Arc<MountedFilesystem>,
}
impl Attachment {
    pub(super) fn new(parent: &Arc<MountView>, point: Arc<Dentry>) -> Self {
        Self {
            parent: Arc::downgrade(parent),
            point,
            _parent_filesystem: parent.filesystem.clone(),
        }
    }
}
/// low-privilege namespace copy 继承不可清除的 flag policy 与不可拆分 attachment。
#[derive(Clone, Copy)]
pub(super) struct MountRestrictions {
    pub(super) flags: MountFlags,
    pub(super) inseparable: bool,
    pub(super) atime_locked: bool,
}
impl Default for MountRestrictions {
    fn default() -> Self {
        Self {
            flags: MountFlags::from_bits(0),
            inseparable: false,
            atime_locked: false,
        }
    }
}
pub(super) struct ViewState {
    pub(super) attachment: Option<Box<super::mount_retirement::Retirement>>,
    pub(super) flags: MountFlags,
    pub(super) writers: usize,
    pub(super) restrictions: MountRestrictions,
    // Exact lifecycle membership token，不作为 topology 的第二份事实。
    pub(super) registration: Option<AttachmentKey>,
    // OWNER: parent 强持实际 children，Attachment 弱指 parent；没有父子 Arc 环。
    pub(super) children: Vec<Arc<MountView>>,
    pub(super) namespace_member: bool,
    pub(super) propagation: super::mount_propagation::Propagation,
    // Linux mnt_expiry_mark：首个 MNT_EXPIRE 设位，普通 path release 清除。
    pub(super) expiry_mark: bool,
    // 只在持 VFS gate 的无分配退休遍历中使用；不属于可查询的 attachment/topology。
    pub(super) retirement_next: Option<Arc<MountView>>,
}
/// 实际 attachment、mnt flags/writers 在同一 owner；filesystem/dentry 可由其他 view 共享。
pub(super) struct MountPayload {
    pub(super) id: u64,
    pub(super) scope: Arc<MountScope>,
    // 每个 view 的实际 mnt_devname；同一 superblock 的独立 mount 可使用不同 source 名称。
    pub(super) source: Arc<Vec<u8>>,
    // 构造 allocation 失败也执行字段析构；实际 edges/root 必须先于 filesystem 释放。
    pub(super) state: Mutex<ViewState>,
    pub(super) root: Arc<Dentry>,
    pub(super) filesystem: Arc<MountedFilesystem>,
    // 普通 umount 的唯一 final resource completion，构造时预留，不依赖退休时 OOM 分配。
    pub(super) complete: Arc<crate::sync::TaskEvent>,
}
impl MountPayload {
    /// cleanup_mnt：先 dput root/placement，再 deactivate filesystem；不能依赖字段声明次序。
    /// bind file 可能已 unlink，最后 root Drop 必须仍能执行实际 orphan reclamation/writeback。
    pub(super) fn release(self) {
        let Self {
            id: _,
            scope,
            source,
            filesystem,
            root,
            state,
            complete,
        } = self;
        drop(state.into_inner());
        drop(root);
        drop(filesystem);
        drop(source);
        drop(scope);
        complete.signal();
    }
}
/// Arc 的最终析构只提交构造时预留的真实 payload；资源 cleanup 在统一拓扑 gate 后执行。
pub(super) struct MountView {
    retirement: Option<Box<super::mount_retirement::Retirement>>,
}
impl core::ops::Deref for MountView {
    type Target = MountPayload;
    fn deref(&self) -> &Self::Target {
        self.retirement.as_ref().expect("live mount payload").view()
    }
}
impl core::fmt::Debug for MountView {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountView")
            .field("namespace", &self.scope.id)
            .finish_non_exhaustive()
    }
}
/// 三种实际配置事务；superblock-only umount-root 不重写 per-view 属性。
pub(super) enum Reconfiguration {
    View(super::MountFlags),
    MountAndSuperblock(super::MountFlags),
    SuperblockReadOnly,
}
impl MountView {
    /// 唯一namespace membership退休：forest仍保活所有master时先注销关系，随后归还实际count。
    pub(super) fn retire_namespace(self: &Arc<Self>) {
        if !self.state.lock().namespace_member {
            return;
        }
        super::mount_propagation::retire(self);
        self.state.lock().namespace_member = false;
        self.scope.retire_mount();
    }
    /// 实际owning forest的parent-first快照；仅当前gate事务临时pin，没有第二份持久topology。
    pub(super) fn snapshot_subtree(self: &Arc<Self>) -> Result<Vec<Arc<Self>>, FileSystemError> {
        let mut views = Vec::new();
        views
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        views.push(self.clone());
        let mut index = 0;
        while index < views.len() {
            let view = views[index].clone();
            let count = view.state.lock().children.len();
            views
                .try_reserve(count)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            views.extend(view.state.lock().children.iter().cloned());
            index += 1;
        }
        Ok(views)
    }
    pub(super) fn new(
        scope: Arc<MountScope>,
        source: Arc<Vec<u8>>,
        filesystem: Arc<MountedFilesystem>,
        root: Arc<Dentry>,
        attachment: Option<Attachment>,
        flags: MountFlags,
        restrictions: MountRestrictions,
    ) -> Result<Arc<Self>, FileSystemError> {
        let attachment = attachment
            .map(super::mount_retirement::Retirement::for_attachment)
            .transpose()?;
        let complete = Arc::try_new(crate::sync::TaskEvent::new())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let payload = MountPayload {
            complete,
            id: NEXT_MOUNT_ID
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .map_err(|_| FileSystemError::OutOfMemory)?,
            scope,
            source,
            filesystem,
            root,
            state: Mutex::new(ViewState {
                attachment,
                flags,
                writers: 0,
                restrictions,
                registration: None,
                children: Vec::new(),
                namespace_member: false,
                propagation: super::mount_propagation::Propagation::default(),
                expiry_mark: false,
                retirement_next: None,
            }),
        };
        let retirement = super::mount_retirement::Retirement::for_view(payload)?;
        Arc::try_new(Self {
            retirement: Some(retirement),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    pub(super) fn attachment_key(&self) -> AttachmentKey {
        match &self.state.lock().attachment {
            Some(edge) => AttachmentKey::Child {
                parent: edge.attachment().parent.as_ptr() as usize,
                point: Arc::as_ptr(&edge.attachment().point) as usize,
            },
            None => AttachmentKey::Root(self.scope.id),
        }
    }
    pub(super) fn attachment(&self) -> Option<Attachment> {
        self.state
            .lock()
            .attachment
            .as_ref()
            .map(|edge| edge.attachment().clone())
    }
    pub(super) fn flags(&self) -> MountFlags {
        // 锁序固定 superblock→mount；二者的 read-only policy 一起决定真实 view 的最终属性。
        let superblock = self.filesystem.state.lock();
        let flags = self.state.lock().flags;
        MountFlags::from_bits(flags.bits() | u64::from(superblock.read_only))
    }
    pub(super) fn acquire_writer(self: &Arc<Self>) -> Result<WriteOpen, FileSystemError> {
        let _admission = self
            .filesystem
            .admission
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut superblock = self.filesystem.state.lock();
        let mut view = self.state.lock();
        if superblock.read_only || view.flags.read_only() {
            return Err(FileSystemError::ReadOnly);
        }
        superblock.writers = superblock
            .writers
            .checked_add(1)
            .expect("superblock writer count overflow");
        view.writers = view
            .writers
            .checked_add(1)
            .expect("mount writer count overflow");
        Ok(WriteOpen {
            view: Some(self.clone()),
        })
    }
    /// touch_atime 的非等待 write admission；冻结/reconfigure 或 RO 时直接跳过更新。
    pub(super) fn try_access_writer(self: &Arc<Self>) -> Option<WriteOpen> {
        let _admission = self.filesystem.admission.try_lock()?;
        let mut superblock = self.filesystem.state.lock();
        let mut view = self.state.lock();
        if superblock.read_only || view.flags.read_only() {
            return None;
        }
        superblock.writers = superblock
            .writers
            .checked_add(1)
            .expect("superblock writer overflow");
        view.writers = view.writers.checked_add(1).expect("mount writer overflow");
        Some(WriteOpen {
            view: Some(self.clone()),
        })
    }
    /// writer grant 与实际后端重配置串行，全部准备成功后一次发布最终属性。
    /// bind remount 仅修改 view；normal remount 的 superblock RO 检查所有 namespace writer。
    ///
    /// # Errors
    ///
    /// RO 转换有写者返回 Busy；准入等待 OOM 或后端准备错误保留原有 mount 属性。
    pub(super) fn reconfigure(
        &self,
        request: Reconfiguration,
        options: &[u8],
        identity: &super::AccessIdentity,
    ) -> Result<(), FileSystemError> {
        let (view_flags, superblock_change, flags) = match request {
            Reconfiguration::View(flags) => (Some(flags), false, flags),
            Reconfiguration::MountAndSuperblock(flags) => (Some(flags), true, flags),
            Reconfiguration::SuperblockReadOnly => (
                None,
                true,
                MountFlags::from_bits(
                    self.state.lock().flags.bits() | u64::from(MountFlags::READ_ONLY),
                ),
            ),
        };
        if superblock_change
            && !identity.namespace_capable(
                &self.filesystem.user,
                syscall_abi::capability::Capability::SYS_ADMIN,
            )
        {
            return Err(FileSystemError::PermissionDenied);
        }
        let _admission = self
            .filesystem
            .admission
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let was_read_only = {
            let superblock = self.filesystem.state.lock();
            let view = self.state.lock();
            let fixed = view.restrictions.flags.bits() & !u64::from(MountFlags::ATIME_MASK);
            if flags.bits() & fixed != fixed
                || view_flags.is_some()
                    && view.restrictions.atime_locked
                    && (flags.bits() & u64::from(MountFlags::ATIME_MASK))
                        != (view.flags.bits() & u64::from(MountFlags::ATIME_MASK))
            {
                return Err(FileSystemError::PermissionDenied);
            }
            if flags.read_only()
                && (view.writers != 0 || superblock_change && superblock.writers != 0)
            {
                return Err(FileSystemError::Busy);
            }
            superblock.read_only
        };
        if superblock_change {
            if flags.read_only() && !was_read_only {
                super::super::page_cache::sync_all()?;
                self.filesystem.filesystem.root_inode()?.sync_storage()?;
            }
            self.filesystem.filesystem.remount(
                options,
                &self.filesystem.user,
                !flags.read_only() && was_read_only,
            )?;
        }
        // admission gate 禁止新写者；release 只会降低 count。这里不再调用可失败后端。
        let mut superblock = self.filesystem.state.lock();
        if let Some(flags) = view_flags {
            self.state.lock().flags = flags;
        }
        if superblock_change {
            superblock.read_only = flags.read_only();
        }
        Ok(())
    }
}
/// OFD、swap 与短 metadata operation 共用的实际 view writer lease；不跨 namespace 重定向。
pub(crate) struct WriteOpen {
    view: Option<Arc<MountView>>,
}
impl WriteOpen {
    /// Pseudo/unmounted inode 的显式空 lease；不能用它替代任意已解析 mount view。
    pub(super) fn unmounted() -> Self {
        Self { view: None }
    }
}
impl Drop for WriteOpen {
    fn drop(&mut self) {
        let Some(view) = self.view.take() else {
            return;
        };
        {
            let mut superblock = view.filesystem.state.lock();
            let mut state = view.state.lock();
            superblock.writers = superblock
                .writers
                .checked_sub(1)
                .expect("superblock writer released twice");
            state.writers = state
                .writers
                .checked_sub(1)
                .expect("mount writer released twice");
        }
        drop(view);
    }
}
impl Drop for MountView {
    fn drop(&mut self) {
        let mut retirement = self.retirement.take().expect("mount retired once");
        retirement.identity = self as *const Self as usize;
        super::mount_retirement::enqueue(retirement);
    }
}
