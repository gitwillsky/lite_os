use super::*;
use crate::sync::{TaskMutexGuard, TaskMutexWaitPreparation};

/// 单次 namespace transaction；唯一 guard 串行 mount 拓扑变更、dentry 位置发布（unlink 的
/// mark_unlinked、rename 的 move_entry）与整条 parent/name 路径投影（Linux `namespace_sem` 与
/// `rename_lock` 的合并）。路径遍历与 filesystem 目录操作不持有它：持有期间不调用可能等待用户态
/// 的 filesystem 操作（FUSE），否则等待中的请求会挡住守护进程自己的每一次路径查找。
/// 锁序在目录锁（[`Dentry`] 的 `lock`）之后。
/// 活 namespace 的强 forest 保活 parent；已 detached 旧路径的弱 parent 可在锁外失效，
/// 每次升级取得的 actual Arc 只保证该次投影的生存期，不把旧 detached tree 永久 pin 住。
pub(super) struct NamespaceAccess<'a> {
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
            _guard: guard,
            _retired: super::mount_retirement::drain(),
        })
    }
    /// 已提交的 filesystem 变更之后发布 dentry 位置：使用预分配 waiter，不会失败。
    ///
    /// `preparation` 必须在 backend commit 前分配；返回持有拓扑/位置发布锁的唯一 capability。
    pub(super) fn namespace_prepared(
        &self,
        preparation: &mut TaskMutexWaitPreparation,
    ) -> NamespaceAccess<'_> {
        NamespaceAccess {
            _guard: self.namespace_mutation.lock_prepared(preparation),
            _retired: super::mount_retirement::drain(),
        }
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
