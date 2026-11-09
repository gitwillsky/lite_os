//! 配置文件提交与原文件撤销。沿用存储能力，不引入另一份配置文件或自定义存储后端。

use alloc::vec::Vec;
use filesystem_protocol::StorageService;
use native_runtime::native_abi::Status;

/// 原提交失败与原文件恢复的独立结果；恢复未确认时把前像所有权交回协调者。
pub(super) struct PersistenceFailure {
    pub(super) status: Status,
    pub(super) restore_error: Option<Status>,
    pub(super) before: Option<Vec<u8>>,
}

/// 恢复原始字节或原来的不存在状态；成功意味着相应目录变更也已同步。
pub(super) fn restore(
    store: &impl StorageService,
    name: &[u8],
    before: Option<&[u8]>,
) -> Result<(), Status> {
    match before {
        // 例如 ENOSPC 发生在 rename 前：原字节仍在时只同步命名状态，不能为无变化的
        // 文件再次申请一份临时空间，否则撤销本身会被同一个磁盘满条件永久阻断。
        Some(bytes) if store.read(name)?.as_deref() == Some(bytes) => store.sync(),
        Some(bytes) => store.write(name, bytes),
        None => store.remove(name),
    }
}

/// 写失败也可能已经替换文件，因此总是撤销；撤销失败不能伪装成普通提交失败。
pub(super) fn persist(
    store: &impl StorageService,
    name: &[u8],
    before: Option<Vec<u8>>,
    bytes: &[u8],
) -> Result<(), PersistenceFailure> {
    match store.write(name, bytes) {
        Ok(()) => Ok(()),
        Err(status) => {
            let restore_error = restore(store, name, before.as_deref()).err();
            Err(PersistenceFailure {
                status,
                restore_error,
                before,
            })
        }
    }
}
