//! 全部 live mount attachment 的精确 lookup/lifecycle index；namespace topology 不复制这里的内容。
use super::{FileSystemError, dentry::Dentry, mount_view::MountView};
use crate::fallible_tree::{FallibleMap, VacantEntry};
use alloc::sync::{Arc, Weak};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum AttachmentKey {
    Root(u64),
    Child { parent: usize, point: usize },
}
pub(super) type PreparedAttachment = VacantEntry<AttachmentKey, Weak<MountView>>;
// OWNER: parent-view/dentry identity→child view；Weak 不延长 namespace 或 physical lease。
// 所有 graph mutation 外层持 VFS namespace transaction；最终析构仅执行 exact pointer retirement。
static ATTACHMENTS: Mutex<FallibleMap<AttachmentKey, Weak<MountView>>> =
    Mutex::new(FallibleMap::new());

pub(super) fn prepare(view: &Arc<MountView>) -> Result<PreparedAttachment, FileSystemError> {
    prepare_at(view, view.attachment_key())
}
/// move 先准备新位置的 membership；在未改变原 attachment 时即可返回 OOM。
pub(super) fn prepare_at(
    view: &Arc<MountView>,
    key: AttachmentKey,
) -> Result<PreparedAttachment, FileSystemError> {
    FallibleMap::try_prepare(key, Arc::downgrade(view)).map_err(|_| FileSystemError::OutOfMemory)
}
/// 在 namespace 发布前，全部节点与实际 views 已准备；单次 graph lock 无失败地提交整组。
pub(super) fn publish(views: &[Arc<MountView>], prepared: &mut [Option<PreparedAttachment>]) {
    assert_eq!(
        views.len(),
        prepared.len(),
        "mount publication preparation count"
    );
    let mut entries = ATTACHMENTS.lock();
    // Caller 的 borrowed view slice 持有每个实际 owner；guard 内不产生临时 Arc 或 final Drop。
    for (view, node) in views.iter().zip(prepared.iter()) {
        let node = node.as_ref().expect("prepared mount node consumed twice");
        assert!(
            core::ptr::eq(node.value().as_ptr(), Arc::as_ptr(view)),
            "mount publication owner mismatch"
        );
        assert!(
            !entries.contains_key(&view.attachment_key()),
            "mount attachment already occupied"
        );
        assert!(
            view.state.lock().registration.is_none(),
            "mount view admitted twice"
        );
    }
    for (view, node) in views.iter().zip(prepared.iter_mut()) {
        let key = view.attachment_key();
        entries.commit_vacant(node.take().expect("prevalidated mount node"));
        let mut state = view.state.lock();
        state.registration = Some(key);
        if !state.namespace_member {
            view.scope.admit_mount();
        }
        state.namespace_member = true;
    }
}

pub(super) fn child(parent: &Arc<MountView>, point: &Arc<Dentry>) -> Option<Arc<MountView>> {
    let key = AttachmentKey::Child {
        parent: Arc::as_ptr(parent) as usize,
        point: Arc::as_ptr(point) as usize,
    };
    ATTACHMENTS.lock().get(&key).and_then(Weak::upgrade)
}
/// preflight只取实际Weak membership；比较identity不升级/保活filesystem，不修改索引。
pub(super) fn owner_at(key: AttachmentKey) -> Option<Weak<MountView>> {
    ATTACHMENTS.lock().get(&key).cloned()
}
pub(super) fn detach(view: &Arc<MountView>) {
    let key = view.state.lock().registration;
    let Some(key) = key else {
        return;
    };
    let retired = {
        let mut entries = ATTACHMENTS.lock();
        if entries
            .get(&key)
            .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), Arc::as_ptr(view)))
        {
            let retired = entries.take_entry(&key);
            assert_eq!(view.state.lock().registration.take(), Some(key));
            retired
        } else {
            None
        }
    };
    drop(retired);
}
pub(super) fn unregister(key: AttachmentKey, view: *const MountView) {
    let retired = {
        let mut entries = ATTACHMENTS.lock();
        if entries
            .get(&key)
            .is_some_and(|entry| core::ptr::eq(entry.as_ptr(), view))
        {
            entries.take_entry(&key)
        } else {
            None
        }
    };
    drop(retired);
}
