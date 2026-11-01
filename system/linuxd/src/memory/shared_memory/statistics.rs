//! 实际已发布共享页的物理 residency aggregate；只由 canonical PageState 事务变更。
use spin::Mutex;

/// 由实际 physical backing 派生的计数输入；不保存另一份 page 状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::memory) enum SharedPageResidency {
    Resident,
    Swapped,
    Discarded,
}

/// 每个 anonymous shared backing 的唯一统计账本。
///
/// PageState → 此短锁；查询只取此锁，不反向取得页表、backing pages 或 PageOp。
/// 页在 backing 索引的无失败 publication 中取得 membership，候选/observer 不另收费。
/// 一次状态转换同锁撤销旧类并加入新类，因此 snapshot 不会观察换出时虚假的总页数。
#[derive(Debug)]
pub(in crate::memory) struct SharedPageStatistics {
    counts: Mutex<(usize, usize)>,
}
impl SharedPageStatistics {
    pub(in crate::memory) fn new() -> Self {
        Self {
            counts: Mutex::new((0, 0)),
        }
    }
    pub(in crate::memory) fn snapshot(&self) -> (usize, usize) {
        *self.counts.lock()
    }
    pub(in crate::memory) fn admit(&self, residency: SharedPageResidency) {
        Self::add(&mut self.counts.lock(), residency);
    }
    pub(in crate::memory) fn retire(&self, residency: SharedPageResidency) {
        Self::remove(&mut self.counts.lock(), residency);
    }
    pub(in crate::memory) fn transition(
        &self,
        previous: SharedPageResidency,
        next: SharedPageResidency,
    ) {
        let mut counts = self.counts.lock();
        Self::remove(&mut counts, previous);
        Self::add(&mut counts, next);
    }
    fn add(counts: &mut (usize, usize), residency: SharedPageResidency) {
        let count = match residency {
            SharedPageResidency::Resident => &mut counts.0,
            SharedPageResidency::Swapped => &mut counts.1,
            SharedPageResidency::Discarded => return,
        };
        *count = count
            .checked_add(1)
            .expect("shared page statistics exhausted");
    }
    fn remove(counts: &mut (usize, usize), residency: SharedPageResidency) {
        let count = match residency {
            SharedPageResidency::Resident => &mut counts.0,
            SharedPageResidency::Swapped => &mut counts.1,
            SharedPageResidency::Discarded => return,
        };
        *count = count
            .checked_sub(1)
            .expect("shared page membership retired twice");
    }
}
