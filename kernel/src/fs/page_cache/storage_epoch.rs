//! page-cache 文件内容的 seqlock 式版本号：让不持 `operation` 的异步填页检测读期间发生的内容改写。
//!
//! 改写者（write/append、truncate、allocate）在持有 `operation` 期间让版本为奇数，结束后变回偶数；
//! 读者在读存储之前取偶数快照，发布之前确认版本没变。三种交错的结果：
//!
//! - 改写者先开始、读者之后取快照：快照为奇数，读者放弃（`stable` 返回 `None`）；
//! - 读者先取快照、改写者随后开始：版本变了，读者丢弃已读到的数据重来；
//! - 改写者已结束、读者之后取快照：读者读到的是改写之后的内容，可以发布。

use core::sync::atomic::{AtomicU64, Ordering};

pub(super) struct StorageEpoch(AtomicU64);

/// 改写者持有期间版本为奇数的 RAII 标记。
pub(super) struct Mutation<'a>(&'a StorageEpoch);

impl StorageEpoch {
    pub(super) const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    /// 当前没有改写者运行时的版本快照。
    pub(super) fn stable(&self) -> Option<u64> {
        let epoch = self.0.load(Ordering::Acquire);
        (epoch & 1 == 0).then_some(epoch)
    }

    /// `snapshot` 之后没有任何改写者开始过。
    pub(super) fn is_current(&self, snapshot: u64) -> bool {
        self.0.load(Ordering::Acquire) == snapshot
    }

    /// 开始一次改写；必须在已持有 `operation` 之后、第一次存储改动之前调用。
    pub(super) fn begin_mutation(&self) -> Mutation<'_> {
        self.0.fetch_add(1, Ordering::AcqRel);
        Mutation(self)
    }
}

impl Drop for Mutation<'_> {
    fn drop(&mut self) {
        self.0.0.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::StorageEpoch;

    #[test]
    fn snapshot_is_refused_while_a_mutation_runs() {
        let epoch = StorageEpoch::new();
        let mutation = epoch.begin_mutation();
        assert_eq!(epoch.stable(), None);
        drop(mutation);
        assert!(epoch.stable().is_some());
    }

    #[test]
    fn a_fill_that_started_before_a_mutation_is_rejected_after_it() {
        let epoch = StorageEpoch::new();
        let snapshot = epoch.stable().unwrap();
        assert!(epoch.is_current(snapshot));
        let mutation = epoch.begin_mutation();
        // 改写者仍在运行：读者发布前的校验必须失败。
        assert!(!epoch.is_current(snapshot));
        drop(mutation);
        // 改写者已结束：版本不会回到旧快照，读到的可能是改写之前的数据，仍然必须失败。
        assert!(!epoch.is_current(snapshot));
    }

    #[test]
    fn a_fill_that_started_after_a_mutation_completed_is_accepted() {
        let epoch = StorageEpoch::new();
        drop(epoch.begin_mutation());
        let snapshot = epoch.stable().unwrap();
        assert!(epoch.is_current(snapshot));
    }

    #[test]
    fn every_mutation_yields_a_distinct_stable_epoch() {
        let epoch = StorageEpoch::new();
        let mut seen = alloc::vec::Vec::new();
        for _ in 0..4 {
            seen.push(epoch.stable().unwrap());
            drop(epoch.begin_mutation());
        }
        seen.dedup();
        assert_eq!(seen.len(), 4);
    }
}
