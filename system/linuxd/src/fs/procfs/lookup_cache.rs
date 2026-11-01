//! proc_fill_cache 等价的目录 child-inode 生命周期；canonical admission 仍归 ProcIdentities。
use super::{DirectoryEntry, FileSystemError, ProcInode, ProcNode};
use crate::fallible_tree::FallibleMap;
use alloc::sync::Arc;
use spin::Mutex;

// 一批 getdents 可表达的最大 entry 数作为目录缓存窗口；它只限制可回收 cache pins，
// 不限制目录内容。超额移除最早签发的 inode；打开的 OFD/live alias 仍 pin canonical inode。
// 缺少窗口会把进程/fd churn 的历史 inode 永久保留到 proc unmount。
const LOOKUP_CACHE_ENTRIES: usize =
    crate::fs::MAX_GETDENTS_BATCH_BYTES / core::mem::size_of::<DirectoryEntry<'static>>();

/// 仅存真正向下的 child pins；inode 只持 Weak canonical index，整个 parent->child 图无环。
/// node 分配、淘汰 node/Arc 的析构均在短锁外；缓存不取得 graph/Task 或 namespace locks。
pub(super) struct ProcLookupCache(Mutex<FallibleMap<(ProcNode, u64), Arc<ProcInode>>>);
impl ProcLookupCache {
    pub(super) fn new() -> Self {
        const { assert!(LOOKUP_CACHE_ENTRIES != 0) };
        Self(Mutex::new(FallibleMap::new()))
    }
    /// 保留已经通过 source revalidation/canonical admission 的实际 child；不另建 inode/number/属性。
    ///
    /// # Errors
    ///
    /// 新缓存节点 OOM 返回 OutOfMemory，既有 child/缓存保持不变。
    pub(super) fn remember(&self, child: &Arc<ProcInode>) -> Result<(), FileSystemError> {
        if self.0.lock().contains_key(&child.key) {
            return Ok(());
        }
        let prepared = FallibleMap::try_prepare(child.key, child.clone())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let (retired, unused) = {
            let mut entries = self.0.lock();
            if entries.contains_key(&child.key) {
                (None, Some(prepared))
            } else {
                let oldest = (entries.len() >= LOOKUP_CACHE_ENTRIES).then(|| {
                    *entries
                        .iter()
                        .min_by_key(|(_, inode)| inode.number)
                        .expect("full child cache")
                        .0
                });
                let retired = oldest.and_then(|key| entries.take_entry(&key));
                entries.commit_vacant(prepared);
                (retired, None)
            }
        };
        drop(retired);
        drop(unused);
        Ok(())
    }
}
