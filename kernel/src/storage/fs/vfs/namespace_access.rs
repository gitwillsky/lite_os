use super::*;
use crate::sync::TaskMutexGuard;

/// 单次 namespace read/mutation transaction；唯一 guard 冻结 inode lookup、opened
/// membership admission、实际 placement 修改与 canonical parent/name projection。
/// 活 namespace 的强 forest 保活 parent；已 detached 旧路径的弱 parent 可在锁外失效，
/// 每次升级取得的 actual Arc 只保证该次 walk 的生存期，不把旧 detached tree 永久 pin 住。
/// 嵌套 traversal 只能借此 capability，
/// 不得再次获取 gate；否则 proc magic link 或 O_CREAT 会递归等待自己。
pub(super) struct NamespaceAccess<'a> {
    pub(super) vfs: &'a VirtualFileSystem,
    _guard: TaskMutexGuard<'a, ()>,
    // 必须后于 guard 析构；filesystem/dentry/final child resource cleanup 均在 gate 外。
    _retired: super::mount_retirement::RetiredBatch,
}
impl VirtualFileSystem {
    pub(super) fn namespace(&self) -> Result<NamespaceAccess<'_>, FileSystemError> {
        let guard = self
            .namespace_mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(NamespaceAccess {
            vfs: self,
            _guard: guard,
            _retired: super::mount_retirement::drain(),
        })
    }
}
impl NamespaceAccess<'_> {
    /// 整条 parent/name 链在同一 namespace transaction 内投影，不混读 rename 前后状态。
    pub(super) fn path(
        &self,
        opened: &Arc<OpenedFile>,
        root: &Arc<OpenedFile>,
        proc: bool,
    ) -> Result<Vec<u8>, FileSystemError> {
        opened.path(root, proc)
    }
}
