//! Live pseudo inode admission：同一 filesystem/inode 的 alias 只读取一个属性 owner。
use super::FileSystemError;
use crate::fallible_tree::FallibleMap;
use alloc::sync::{Arc, Weak};
use spin::Mutex;

/// Weak 表只保留 live inode identity，不延长 namespace、Task 或打开文件的寿命。
/// 节点与 candidate 在锁外分配；摘除节点、loser 和最后一个 Weak 都在锁外析构。
pub(super) struct LiveInodes<T, K: Ord + Copy>(Mutex<FallibleMap<K, Weak<T>>>);
impl<T, K: Ord + Copy> LiveInodes<T, K> {
    pub(super) const fn new() -> Self {
        Self(Mutex::new(FallibleMap::new()))
    }

    /// Pin 仍存活的实际 inode；仅做 O(log n) lookup 和 Arc increment。
    pub(super) fn get(&self, number: K) -> Option<Arc<T>> {
        self.0.lock().get(&number).and_then(Weak::upgrade)
    }

    /// 锁外准备新 identity，锁内复查并选择唯一 winner；OOM 不发布 inode。
    pub(super) fn admit(&self, number: K, candidate: &Arc<T>) -> Result<Arc<T>, FileSystemError> {
        if let Some(live) = self.get(number) {
            return Ok(live);
        }
        let prepared = FallibleMap::try_prepare(number, Arc::downgrade(candidate))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let (selected, retired, unused) = {
            let mut table = self.0.lock();
            if let Some(live) = table.get(&number).and_then(Weak::upgrade) {
                (live, None, Some(prepared))
            } else {
                let retired = table.take_entry(&number);
                table.commit_vacant(prepared);
                (candidate.clone(), retired, None)
            }
        };
        drop(retired);
        drop(unused);
        Ok(selected)
    }

    /// 实际 inode 的 Drop 摘除自己的 entry；迟到的 loser 不得删除新 winner。
    pub(super) fn retire(&self, number: K, owner: &T) {
        let retired = {
            let mut table = self.0.lock();
            if table
                .get(&number)
                .is_some_and(|live| core::ptr::eq(live.as_ptr(), owner))
            {
                table.take_entry(&number)
            } else {
                None
            }
        };
        drop(retired);
    }
}
