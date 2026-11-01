//! 实际 mount view 的创建、迁移与路径 traversal；持同一 namespace mutation transaction。
use super::*;

impl VirtualFileSystem {
    /// Explicit caller namespace，mountpoint 的实际 view 必须在该 namespace 内。
    pub(in crate::fs) fn mount(
        &self,
        namespace: &Arc<MountNamespace>,
        point: Arc<OpenedFile>,
        filesystem: Arc<MountedFilesystem>,
        source: Arc<Vec<u8>>,
        root_inode: Arc<dyn Inode>,
        flags: MountFlags,
    ) -> Result<(), FileSystemError> {
        self.lock_mount(point, |point| {
            let parent = point
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?;
            if !namespace.contains(parent) {
                return Err(FileSystemError::InvalidOperation);
            }
            if point.inode().inode_type() != InodeType::Directory
                || root_inode.inode_type() != InodeType::Directory
            {
                return Err(FileSystemError::NotDirectory);
            }
            let dentry = self.dentries.admit(Dentry::named(root_inode, None, b"")?)?;
            let view = MountView::new(
                namespace.scope.clone(),
                source,
                filesystem.clone(),
                dentry,
                Some(Attachment::new(parent, point.dentry.clone())),
                flags,
                MountRestrictions::default(),
            )?;
            super::mount_attachment::attach(core::slice::from_ref(&view), point)
        })
    }
    /// MS_BIND 创建实际新 view，共享 superblock 与 canonical subtree root，不复制 filesystem。
    /// 所有节点、容量与 membership 先准备；target 必须属于调用者 namespace；source须为调用者当前namespace中的普通mounted view。
    pub(crate) fn bind_mount(
        &self,
        namespace: &Arc<MountNamespace>,
        source: &Arc<OpenedFile>,
        point: &Arc<OpenedFile>,
        recursive: bool,
    ) -> Result<(), FileSystemError> {
        self.lock_mount(point.clone(), |point| {
            let origin = source
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?;
            let namespace_file = source.inode().namespace_reference().is_some();
            let parent = point
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?;
            if !namespace.contains(parent) || !namespace_file && !namespace.contains(origin) {
                return Err(FileSystemError::InvalidOperation);
            }
            if namespace.namespace_file_loops(&*source.inode()) {
                // 固定Linux do_loopback的此项拒绝是EINVAL；move子树检查使用ELOOP。
                return Err(FileSystemError::InvalidOperation);
            }
            if (source.inode().inode_type() == InodeType::Directory)
                != (point.inode().inode_type() == InodeType::Directory)
            {
                return Err(FileSystemError::NotDirectory);
            }
            if mount_propagation::unbindable(origin) {
                return Err(FileSystemError::InvalidOperation);
            }
            let mut selected = Vec::new();
            selected
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            selected.push(origin.clone());
            for old in origin.snapshot_subtree()? {
                if Arc::ptr_eq(&old, origin) {
                    continue;
                }
                let old_root = OpenedFile::at(old.clone(), old.root.clone())?;
                if !old_root.project_path(source, false)?.1 {
                    continue;
                }
                if !recursive && old.state.lock().restrictions.inseparable {
                    return Err(FileSystemError::InvalidOperation);
                }
                if recursive {
                    let mut ancestor = Some(old.clone());
                    let mut pruned = false;
                    while let Some(view) = ancestor {
                        if Arc::ptr_eq(&view, origin) {
                            break;
                        }
                        if mount_propagation::unbindable(&view) {
                            if view.state.lock().restrictions.inseparable {
                                return Err(FileSystemError::PermissionDenied);
                            }
                            pruned = true;
                            break;
                        }
                        ancestor = view.attachment().and_then(|edge| edge.parent.upgrade());
                    }
                    if pruned {
                        continue;
                    }
                    selected
                        .try_reserve(1)
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    selected.push(old);
                }
            }
            let views = super::mount_copy::copy_tree(
                &selected,
                super::mount_copy::CopyPolicy {
                    scope: namespace.scope.clone(),
                    root: source.dentry.clone(),
                    attachment: Some(Attachment::new(parent, point.dentry.clone())),
                    slave: false,
                    shared: false,
                    lock_tree: false,
                    prepared_shared: &[],
                },
            )?;
            super::mount_attachment::attach(&views, point)
        })
    }
    /// MS_MOVE 迁移同一 view identity；失败不会先摘除旧位置或修改任何 parent。
    pub(crate) fn move_mount(
        &self,
        namespace: &Arc<MountNamespace>,
        source: &Arc<OpenedFile>,
        point: &Arc<OpenedFile>,
    ) -> Result<(), FileSystemError> {
        self.lock_mount(point.clone(), |point| {
            let view = source
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?;
            let parent = point
                .mount
                .as_ref()
                .ok_or(FileSystemError::InvalidOperation)?;
            if !namespace.contains(view)
                || !namespace.contains(parent)
                || !Arc::ptr_eq(&source.dentry, &view.root)
                || view.attachment().is_none()
                || view.state.lock().restrictions.inseparable
            {
                return Err(FileSystemError::InvalidOperation);
            }
            if (source.inode().inode_type() == InodeType::Directory)
                != (point.inode().inode_type() == InodeType::Directory)
            {
                return Err(FileSystemError::NotDirectory);
            }
            let old_parent = view
                .attachment()
                .and_then(|edge| edge.parent.upgrade())
                .ok_or(FileSystemError::InvalidOperation)?;
            if mount_propagation::group(&old_parent).is_some() {
                return Err(FileSystemError::InvalidOperation);
            }
            let tree = view.snapshot_subtree()?;
            if tree
                .iter()
                .any(|view| namespace.namespace_file_loops(&*view.root.inode()))
            {
                return Err(FileSystemError::Loop);
            }
            super::mount_attachment::attach(&tree, point)
        })
    }
    /// Linux `lock_mount`：独占持有实际顶层挂载点 dentry，再取得 namespace gate 执行 `attach`。
    ///
    /// 1. 旧 dirfd/magic path 可能仍指被覆盖的下层，先进入实际顶层 mount；
    /// 2. 等待挂载点锁期间别人先挂到同一位置时，换到新的顶层重试；
    /// 3. 挂载点已被删除为 ENOENT。持有挂载点锁使 unlink/rename 的挂载点检查与本次挂载互斥。
    fn lock_mount<R>(
        &self,
        point: Arc<OpenedFile>,
        attach: impl FnOnce(&Arc<OpenedFile>) -> Result<R, FileSystemError>,
    ) -> Result<R, FileSystemError> {
        let mut point = self.enter_mount(point)?;
        loop {
            let top = {
                let _point = point.dentry.lock_exclusive()?;
                let transaction = self.namespace()?;
                let top = transaction.enter_mount(point.clone())?;
                if top.same_path(&point) {
                    if point.is_unlinked() {
                        return Err(FileSystemError::NotFound);
                    }
                    return attach(&point);
                }
                top
            };
            point = top;
        }
    }
    /// 路径沿所属 tree 的 attachment 展开；setns 保留的旧 root/dirfd 继续使用其真实 view。
    pub(super) fn enter_mount(
        &self,
        opened: Arc<OpenedFile>,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        // 只在取得实际 mount pin 时持拓扑锁；与普通卸载的 busy 检查、摘索引保持同一事务。
        // 后续 find_child/权限检查在锁外执行，不能把 FUSE 请求的等待带回全局 gate。
        self.namespace()?.enter_mount(opened)
    }
    pub(super) fn is_mount_point(&self, path: &Arc<OpenedFile>) -> bool {
        path.mount.as_ref().is_some_and(|view| {
            Arc::ptr_eq(&path.dentry, &view.root)
                || mount_index::child(view, &path.dentry).is_some()
        })
    }
    pub(crate) fn is_namespace_root(
        &self,
        namespace: &Arc<MountNamespace>,
        opened: &Arc<OpenedFile>,
    ) -> bool {
        let root = namespace.root_view();
        opened
            .mount
            .as_ref()
            .is_some_and(|view| Arc::ptr_eq(&root, view))
            && Arc::ptr_eq(&root.root, &opened.dentry)
    }
    /// 持 gate 根据实际 subtree 修改传播属性；target须为live namespace中的实际挂载根，
    /// caller须在该实际namespace的owning UserNS有SYS_ADMIN（旧namespace dirfd同样用实际owner）。
    /// 全部Shared group先分配，OOM不会改变任何既有关系；recursive不会触及未选中的相邻挂载。
    pub(crate) fn change_mount_propagation(
        &self,
        authority: &crate::namespace::NamespaceAuthority,
        root: &Arc<OpenedFile>,
        kind: MountPropagation,
        recursive: bool,
    ) -> Result<(), FileSystemError> {
        let _transaction = self.namespace()?;
        let view = root
            .mount
            .as_ref()
            .ok_or(FileSystemError::InvalidOperation)?;
        if !view.state.lock().namespace_member || !Arc::ptr_eq(&root.dentry, &view.root) {
            return Err(FileSystemError::InvalidOperation);
        }
        if !authority.capable(
            &view.scope.user,
            syscall_abi::capability::Capability::SYS_ADMIN,
        ) {
            return Err(FileSystemError::PermissionDenied);
        }
        let views = if recursive {
            view.snapshot_subtree()?
        } else {
            let mut views = Vec::new();
            views
                .try_reserve_exact(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            views.push(view.clone());
            views
        };
        mount_propagation::change(&views, kind)?;
        view.scope.changed();
        Ok(())
    }
}

impl super::namespace_access::NamespaceAccess<'_> {
    /// 在 caller 已持有的拓扑事务中进入实际顶层 mount，不调用 filesystem I/O。
    ///
    /// `opened` 是待穿越的实际路径；返回已取得 mount pin 的路径。路径分配失败返回 OutOfMemory。
    pub(super) fn enter_mount(
        &self,
        mut opened: Arc<OpenedFile>,
    ) -> Result<Arc<OpenedFile>, FileSystemError> {
        while let Some(view) = &opened.mount
            && let Some(child) = mount_index::child(view, &opened.dentry)
        {
            let dentry = child.root.clone();
            opened = OpenedFile::at(child, dentry)?;
        }
        Ok(opened)
    }
}
