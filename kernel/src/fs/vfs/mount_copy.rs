//! Linux clone_mnt/copy_tree 的唯一view构造路径；namespace、bind与传播只选择真实输入树及policy。
use super::{Attachment, Dentry, FileSystemError, MountScope, MountView, mount_propagation};
use alloc::{sync::Arc, vec::Vec};

/// Linux CL_COPY_MNT_NS_FILE未设置时跳过namespace-file及其overmount subtree。
/// 输入来自真实parent-first森林；临时索引只记录此次选择，namespace relocation使用同一选中源序列。
pub(super) fn without_namespace_files(
    source: &[Arc<MountView>],
) -> Result<Vec<Arc<MountView>>, FileSystemError> {
    let mut selected = Vec::new();
    selected
        .try_reserve_exact(source.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let mut included = crate::fallible_tree::FallibleMap::new();
    for (index, view) in source.iter().enumerate() {
        let namespace_file = view
            .root
            .inode()
            .namespace_reference()
            .is_some_and(|reference| reference.kind() == crate::namespace::NamespaceType::Mount);
        let keep = if index == 0 {
            if namespace_file {
                return Err(FileSystemError::InvalidOperation);
            }
            true
        } else {
            let edge = view.attachment().ok_or(FileSystemError::InvalidOperation)?;
            !namespace_file
                && *included
                    .get(&(edge.parent.as_ptr() as usize))
                    .ok_or(FileSystemError::InvalidOperation)?
        };
        included
            .try_insert(Arc::as_ptr(view) as usize, keep)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        if keep {
            selected.push(view.clone());
        }
    }
    Ok(selected)
}

/// 一次复制的immutable准备输入；root/attachment允许bind截取dentry或namespace建立无父root。
pub(super) struct CopyPolicy<'a> {
    pub(super) scope: Arc<MountScope>,
    pub(super) root: Arc<Dentry>,
    pub(super) attachment: Option<Attachment>,
    pub(super) slave: bool,
    pub(super) shared: bool,
    pub(super) lock_tree: bool,
    pub(super) prepared_shared: &'a [mount_propagation::PreparedShared],
}

/// 输入必须为实际parent先于child的选中树；每个输出是新identity，共享真实filesystem/dentry。
/// 所有返回view尚未发表，membership/owning edges在统一Publication中无失败提交；OOM不改变源树。
pub(super) fn copy_tree(
    source: &[Arc<MountView>],
    policy: CopyPolicy<'_>,
) -> Result<Vec<Arc<MountView>>, FileSystemError> {
    if source.is_empty() {
        return Err(FileSystemError::InvalidOperation);
    }
    let mut views = Vec::new();
    views
        .try_reserve_exact(source.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    let mut correspondence = crate::fallible_tree::FallibleMap::new();
    for (index, old) in source.iter().enumerate() {
        let (root, attachment) = if index == 0 {
            (policy.root.clone(), policy.attachment.clone())
        } else {
            let attachment = old.attachment().ok_or(FileSystemError::InvalidOperation)?;
            let parent = correspondence
                .get(&(attachment.parent.as_ptr() as usize))
                .ok_or(FileSystemError::InvalidOperation)?;
            (
                old.root.clone(),
                Some(Attachment::new(parent, attachment.point)),
            )
        };
        let (flags, mut restrictions) = {
            let state = old.state.lock();
            (state.flags, state.restrictions)
        };
        // clone_mnt清除root内部MNT_LOCKED；copy_tree只恢复选中children的锁。
        if index == 0 {
            restrictions.inseparable = false;
        }
        if policy.lock_tree {
            restrictions.flags =
                super::MountFlags::from_bits(restrictions.flags.bits() | flags.bits());
            restrictions.atime_locked = true;
            restrictions.inseparable |= index != 0;
        }
        let view = MountView::new(
            policy.scope.clone(),
            old.source.clone(),
            old.filesystem.clone(),
            root,
            attachment,
            flags,
            restrictions,
        )?;
        mount_propagation::prepare_copy(
            old,
            &view,
            mount_propagation::prepared_group(old, policy.prepared_shared),
            policy.slave,
            policy.shared,
        )?;
        correspondence
            .try_insert(Arc::as_ptr(old) as usize, view.clone())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        views.push(view);
    }
    Ok(views)
}
