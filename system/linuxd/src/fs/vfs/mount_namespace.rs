//! Mount namespace 的实际 topology 与 ucounts 生命周期；filesystem/dentry 由 view 共享。
use super::{
    FileSystemError, LookupOrigin, OpenedFile, mount_index,
    mount_view::{MountScope, MountView},
};
use crate::namespace::{
    NamespaceError, NamespaceOwner, NamespaceType, UserNamespace, UserResource, UserResourceCharge,
};
use alloc::boxed::Box;
use alloc::{sync::Arc, vec::Vec};
use spin::Once;

/// Namespace 强持唯一 forest root，parent 强持 children、attachment 弱指 parent。
/// final Drop 只移交预置退休节点；charge 同步释放，实际拓扑/资源统一在 VFS gate 下退休。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnmountMode {
    Normal,
    Detach,
    Expire,
}
pub(crate) struct MountNamespace {
    pub(super) scope: Arc<MountScope>,
    // OWNER: 初始 namespace 早于 worker；唯一 root forest 安装一次，所有 membership 保持同一 Arc。
    root: Once<Arc<MountView>>,
    retirement: Option<Box<super::mount_retirement::Retirement>>,
    _charge: Option<UserResourceCharge>,
}
impl core::fmt::Debug for MountNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountNamespace")
            .field("id", &self.scope.id)
            .finish_non_exhaustive()
    }
}
impl NamespaceOwner for MountNamespace {
    fn kind(&self) -> NamespaceType {
        NamespaceType::Mount
    }
    fn id(&self) -> u64 {
        self.scope.id
    }
    fn owning_user(&self) -> Option<Arc<UserNamespace>> {
        Some(self.scope.user.clone())
    }
}

/// correspondence 仅在 clone transaction 内使用；事务结束不继续 pin source tree。
fn relocate(
    correspondence: &[(Arc<MountView>, Arc<MountView>)],
    path: &Arc<OpenedFile>,
) -> Result<Arc<OpenedFile>, FileSystemError> {
    let Some(old) = &path.mount else {
        return Ok(path.clone());
    };
    let Some(view) = correspondence
        .iter()
        .find(|(source, _)| Arc::ptr_eq(source, old))
        .map(|(_, target)| target.clone())
    else {
        return Ok(path.clone());
    };
    OpenedFile::at(view, path.dentry.clone())
}
impl MountNamespace {
    /// 创建 boot 初始 namespace owner；root mount 稍后安装，worker 可先捕获 membership。
    pub(super) fn initial(scope: Arc<MountScope>) -> Result<Arc<Self>, FileSystemError> {
        let retirement = super::mount_retirement::Retirement::for_namespace()?;
        Arc::try_new(Self {
            scope,
            root: Once::new(),
            retirement: Some(retirement),
            _charge: None,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
    /// 在 VFS namespace transaction 内一次安装真实 root；失败不替换 namespace identity。
    pub(super) fn install_root(&self, root: Arc<MountView>) -> Result<(), FileSystemError> {
        if self.root.get().is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        self.root
            .try_call_once(|| {
                let mut nodes = [Some(mount_index::prepare(&root)?)];
                self.insert_prepared(core::slice::from_ref(&root), &mut nodes)?;
                Ok(root)
            })
            .map(|_| ())
    }
    pub(super) fn root_view(&self) -> Arc<MountView> {
        self.root.get().expect("live mount namespace root").clone()
    }
    /// 唯一 forest 的实际 child ownership；snapshot 仅在事务内持临时 pins，不成为第二份 topology。
    pub(super) fn snapshot(&self) -> Result<Vec<Arc<MountView>>, FileSystemError> {
        self.root_view().snapshot_subtree()
    }
    pub(crate) fn event(&self) -> u64 {
        self.scope.event()
    }
    pub(crate) fn notification_pipe(&self) -> Arc<crate::ipc::Pipe> {
        self.scope.notification_pipe()
    }
    pub(crate) fn drain_notification(&self) {
        self.scope.drain_notification();
    }
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.scope.user.clone()
    }
    pub(crate) fn root(&self) -> Result<Arc<OpenedFile>, FileSystemError> {
        let view = self.root.get().ok_or(FileSystemError::NotFound)?.clone();
        let dentry = view.root.clone();
        OpenedFile::at(view, dentry)
    }
    /// 首个 view 必须是 root；每个 child 的 parent 先于它准备，提交整组 graph 不再分配。
    /// 所有可失败步骤完成前不发表新 attachment，失败只释放真实未发表 owner 与 charge。
    pub(crate) fn copy_for(
        &self,
        user: Arc<UserNamespace>,
        uid: u32,
        origin: LookupOrigin,
    ) -> Result<(Arc<Self>, LookupOrigin), NamespaceError> {
        let charge = UserResourceCharge::acquire(&user, uid, UserResource::Mount)?;
        let _transaction = super::vfs().namespace().map_err(namespace_error)?;
        let scope = MountScope::new(crate::namespace::next_namespace_id(), user)
            .map_err(namespace_error)?;
        let source =
            super::mount_copy::without_namespace_files(&self.snapshot().map_err(namespace_error)?)
                .map_err(namespace_error)?;
        let count = source.len();
        let lower_user = !Arc::ptr_eq(&scope.user, &self.scope.user);
        let views = super::mount_copy::copy_tree(
            &source,
            super::mount_copy::CopyPolicy {
                scope: scope.clone(),
                root: source.first().expect("namespace root").root.clone(),
                attachment: None,
                slave: lower_user,
                shared: false,
                lock_tree: lower_user,
                prepared_shared: &[],
            },
        )
        .map_err(namespace_error)?;
        let mut nodes = Vec::new();
        let mut correspondence = Vec::new();
        nodes
            .try_reserve_exact(count)
            .map_err(|_| NamespaceError::OutOfMemory)?;
        correspondence
            .try_reserve_exact(count)
            .map_err(|_| NamespaceError::OutOfMemory)?;
        for (old, view) in source.into_iter().zip(views.iter()) {
            nodes.push(Some(mount_index::prepare(view).map_err(namespace_error)?));
            correspondence.push((old, view.clone()));
        }
        let namespace = Arc::try_new(Self {
            scope,
            root: Once::initialized(views.first().expect("copied root view").clone()),
            retirement: Some(
                super::mount_retirement::Retirement::for_namespace().map_err(namespace_error)?,
            ),
            _charge: Some(charge),
        })
        .map_err(|_| NamespaceError::OutOfMemory)?;
        let (root, cwd) = origin.into_parts();
        let root = relocate(&correspondence, &root).map_err(namespace_error)?;
        let cwd = relocate(&correspondence, &cwd).map_err(namespace_error)?;
        namespace
            .insert_prepared(&views, &mut nodes)
            .map_err(namespace_error)?;
        Ok((namespace, LookupOrigin::new(root, cwd)))
    }
    fn insert_prepared(
        &self,
        incoming: &[Arc<MountView>],
        nodes: &mut [Option<mount_index::PreparedAttachment>],
    ) -> Result<(), FileSystemError> {
        assert!(
            incoming
                .iter()
                .all(|view| Arc::ptr_eq(&self.scope, &view.scope)),
            "mount view belongs to another namespace"
        );
        super::mount_publication::Publication::prepare(
            incoming,
            nodes,
            Vec::new(),
            Vec::new(),
            None,
        )?
        .publish();
        Ok(())
    }
    pub(super) fn contains(&self, view: &Arc<MountView>) -> bool {
        Arc::ptr_eq(&self.scope, &view.scope) && view.state.lock().namespace_member
    }
    /// Linux mnt_ns_loop：只允许此mount namespace pin更晚创建的mount namespace，避免强forest环。
    pub(super) fn namespace_file_loops(&self, inode: &dyn super::Inode) -> bool {
        inode.namespace_reference().is_some_and(|reference| {
            reference.kind() == NamespaceType::Mount && self.scope.id >= reference.id()
        })
    }
    /// can_umount 的唯一目标准入；所有 normal/detach/expire/root-RO 分支都先检查实际 membership/root/lock。
    pub(super) fn unmount_view<'a>(
        &self,
        root: &'a Arc<OpenedFile>,
    ) -> Result<&'a Arc<MountView>, FileSystemError> {
        let view = root
            .mount
            .as_ref()
            .ok_or(FileSystemError::InvalidOperation)?;
        if !self.contains(view) || !Arc::ptr_eq(&root.dentry, &view.root) {
            return Err(FileSystemError::InvalidOperation);
        }
        if view.state.lock().restrictions.inseparable {
            return Err(FileSystemError::InvalidOperation);
        }
        Ok(view)
    }
    /// 目标 path 的实际 view 必须属于 caller；检查 root FD 引用与所有 descendant/其他 path pin。
    /// Graph 与 traversal 由外层 namespace transaction 串行，busy check 后不会准入新路径。
    pub(super) fn remove(
        &self,
        root: &Arc<OpenedFile>,
        mode: UnmountMode,
    ) -> Result<Vec<Arc<crate::execution::sync::TaskEvent>>, FileSystemError> {
        let view = self.unmount_view(root)?;
        if Arc::ptr_eq(&self.root_view(), view) {
            return Err(FileSystemError::InvalidOperation);
        }
        if mode != UnmountMode::Detach
            && (!view.state.lock().children.is_empty()
                || Arc::strong_count(view) > 2
                || Arc::strong_count(root) > 1)
        {
            return Err(FileSystemError::Busy);
        }
        if mode == UnmountMode::Expire {
            let mut state = view.state.lock();
            if !core::mem::replace(&mut state.expiry_mark, true) {
                return Err(FileSystemError::Retry);
            }
        }
        super::mount_unmount::unmount(view, mode != UnmountMode::Detach)
    }
}
impl Drop for MountNamespace {
    fn drop(&mut self) {
        let mut retirement = self
            .retirement
            .take()
            .expect("namespace retirement prepared");
        // 移交一个真实 Arc pin；Once 原 pin 随字段析构释放，退休节点独占后续 forest cleanup。
        // 不消费 spin 0.10 Once 的 extraction API：其 read 后仍析构原值，会重复 Drop。
        if let Some(root) = self.root.get().map(Arc::clone) {
            retirement.install_namespace(root);
            super::mount_retirement::enqueue(retirement);
        }
        // 未安装 root 的未发表 owner 没有 forest；retirement token 原样 RAII 释放，不虚构清理任务。
    }
}
fn namespace_error(error: FileSystemError) -> NamespaceError {
    match error {
        FileSystemError::OutOfMemory => NamespaceError::OutOfMemory,
        _ => panic!("mount clone invariant failed: {:?}", error),
    }
}
