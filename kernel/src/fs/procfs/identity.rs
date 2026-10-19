use super::{FileSystemError, ProcNode};

/// 实际 proc live identity owner：有序完整 node/scope key 与 inode 号签发器。
/// weak admission 不延长 Task/namespace 寿命；编号只给候选者，竞争失败留下空号。
pub(super) struct ProcIdentities<T> {
    pub(super) live: super::super::inode_identity::LiveInodes<T, (ProcNode, u64)>,
    next: core::sync::atomic::AtomicU64,
}
impl<T> ProcIdentities<T> {
    pub(super) fn new() -> Self {
        Self {
            live: super::super::inode_identity::LiveInodes::new(),
            next: core::sync::atomic::AtomicU64::new(1),
        }
    }
    pub(super) fn allocate_number(&self) -> Result<u64, FileSystemError> {
        self.next
            .try_update(
                core::sync::atomic::Ordering::Relaxed,
                core::sync::atomic::Ordering::Relaxed,
                |number| number.checked_add(1),
            )
            .map_err(|_| FileSystemError::NoSpace)
    }
}
