//! Linux attach_recursive_mnt：准备所有真实接收树与overmount迁移，再单次发表。
use super::{
    Attachment, Dentry, FileSystemError, MountView, OpenedFile, mount_copy, mount_index,
    mount_propagation, mount_publication,
};
use alloc::{sync::Arc, vec::Vec};

/// 一次attach中已准备的copy来源；range只借incoming，None借实际moving源树。
struct CopySource {
    destination: Arc<MountView>,
    range: Option<core::ops::Range<usize>>,
}
fn peers(left: &Arc<MountView>, right: &Arc<MountView>) -> bool {
    Arc::ptr_eq(left, right)
        || mount_propagation::group(left).is_some_and(|left| {
            mount_propagation::group(right).is_some_and(|right| Arc::ptr_eq(&left, &right))
        })
}
fn visible(point: &Arc<Dentry>, root: &Arc<Dentry>) -> bool {
    let mut cursor = Some(point.clone());
    while let Some(entry) = cursor {
        if Arc::ptr_eq(&entry, root) {
            return true;
        }
        cursor = entry.snapshot().and_then(|(parent, _, _)| parent);
    }
    false
}
fn source_for(
    receiver: &Arc<MountView>,
    copies: &[CopySource],
) -> Result<(usize, bool), FileSystemError> {
    if let Some(index) = copies
        .iter()
        .position(|copy| peers(receiver, &copy.destination))
    {
        return Ok((index, false));
    }
    // 隐藏bind-root可能没有secondary，沿真实master上溯至最近已有copy的peer group。
    // 接收者按master在child之前枚举；不能用任意全局group代表代替可见copy。
    let mut master = mount_propagation::master(receiver);
    while let Some(parent) = master {
        if let Some(index) = copies
            .iter()
            .position(|copy| peers(&parent, &copy.destination))
        {
            return Ok((index, true));
        }
        master = mount_propagation::master(&parent);
    }
    Err(FileSystemError::InvalidOperation)
}
fn overmount(tree: &[Arc<MountView>]) -> Arc<MountView> {
    let mut top = tree[0].clone();
    loop {
        let child = tree.iter().find(|view| {
            view.attachment().is_some_and(|edge| {
                core::ptr::eq(edge.parent.as_ptr(), Arc::as_ptr(&top))
                    && Arc::ptr_eq(&edge.point, &top.root)
            })
        });
        let Some(child) = child else {
            return top;
        };
        top = child.clone();
    }
}

/// Caller持VFS mutation gate；source为parent-first实际树，point已按lock_mount进入顶层。
/// new/bind源尚未发表；move源保留原identity。所有copy/stack/index/容量成功后才改变live关系。
pub(super) fn attach(
    source: &[Arc<MountView>],
    point: &Arc<OpenedFile>,
) -> Result<(), FileSystemError> {
    let root = source.first().ok_or(FileSystemError::InvalidOperation)?;
    let parent = point
        .mount
        .as_ref()
        .ok_or(FileSystemError::InvalidOperation)?;
    let moving = root.state.lock().namespace_member;
    if source.iter().any(|view| {
        view.state.lock().namespace_member != moving || !Arc::ptr_eq(&view.scope, &root.scope)
    }) || !Arc::ptr_eq(&parent.scope, &root.scope)
    {
        return Err(FileSystemError::InvalidOperation);
    }
    let shared = mount_propagation::group(parent).is_some();
    if shared
        && source
            .iter()
            .any(|view| mount_propagation::unbindable(view))
    {
        return Err(FileSystemError::InvalidOperation);
    }
    let prepared_shared = if shared {
        mount_propagation::prepare_shared(source)?
    } else {
        Vec::new()
    };
    let mut incoming = Vec::new();
    if !moving {
        incoming
            .try_reserve_exact(source.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        incoming.extend(source.iter().cloned());
    }
    let mut placements = Vec::new();
    if moving {
        placements
            .try_reserve(1)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        placements.push(mount_publication::Placement::prepare(
            root.clone(),
            Attachment::new(parent, point.dentry.clone()),
        )?);
    }
    let mut copies = Vec::new();
    copies
        .try_reserve(1)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    copies.push(CopySource {
        destination: parent.clone(),
        range: if moving { None } else { Some(0..source.len()) },
    });
    let mut unlock = Vec::new();
    if shared {
        // 在创造任何secondary前pin已有接收者；本事务的staged树不参加传播遍历。
        for receiver in mount_propagation::receivers(parent)? {
            if Arc::ptr_eq(&receiver, parent)
                || !receiver.state.lock().namespace_member
                || !visible(&point.dentry, &receiver.root)
            {
                continue;
            }
            let (origin, slave) = source_for(&receiver, &copies)?;
            let tree = copies[origin]
                .range
                .as_ref()
                .map_or(source, |range| &incoming[range.clone()]);
            let tree = mount_copy::without_namespace_files(tree)?;
            let clone = mount_copy::copy_tree(
                &tree,
                mount_copy::CopyPolicy {
                    scope: receiver.scope.clone(),
                    root: tree[0].root.clone(),
                    attachment: Some(Attachment::new(&receiver, point.dentry.clone())),
                    slave,
                    shared: mount_propagation::group(&receiver).is_some(),
                    lock_tree: !Arc::ptr_eq(&receiver.scope.user, &parent.scope.user),
                    prepared_shared: &prepared_shared,
                },
            )?;
            if let Some(covered) = mount_index::child(&receiver, &point.dentry) {
                let top = overmount(&clone);
                placements
                    .try_reserve(1)
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                placements.push(mount_publication::Placement::prepare(
                    covered.clone(),
                    Attachment::new(&top, top.root.clone()),
                )?);
                if covered.state.lock().restrictions.inseparable {
                    // 锁隐藏职责转交给最下层new root；既有q只在全部preflight成功后解锁。
                    clone[0].state.lock().restrictions.inseparable = true;
                    unlock
                        .try_reserve(1)
                        .map_err(|_| FileSystemError::OutOfMemory)?;
                    unlock.push(covered);
                }
            }
            incoming
                .try_reserve(clone.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            copies
                .try_reserve(1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            let start = incoming.len();
            incoming.extend(clone);
            copies.push(CopySource {
                destination: receiver,
                range: Some(start..incoming.len()),
            });
        }
    }
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(incoming.len())
        .map_err(|_| FileSystemError::OutOfMemory)?;
    for view in &incoming {
        nodes.push(Some(mount_index::prepare(view)?));
    }
    let publication = mount_publication::Publication::prepare(
        &incoming,
        &mut nodes,
        placements,
        prepared_shared,
        None,
    )?;
    for view in unlock {
        view.state.lock().restrictions.inseparable = false;
    }
    publication.publish();
    Ok(())
}
