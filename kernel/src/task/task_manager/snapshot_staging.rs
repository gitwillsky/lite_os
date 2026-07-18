/// 锁外 snapshot storage 与最终 owner 复查的容量决策。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapshotCapacity {
    Capture,
    Retry { minimum: usize },
}

/// @description 比较锁外预留容量与同一次 owner 快照的最终需求。
/// @param capacity 当前空 staging Vec 可无分配写入的元素数。
/// @param required 最终 owner guard 下重新计算的元素数。
/// @return 容量足够时允许 capture；并发增长时要求锁外按新下界重试。
pub(super) const fn snapshot_capacity(capacity: usize, required: usize) -> SnapshotCapacity {
    if capacity < required {
        SnapshotCapacity::Retry { minimum: required }
    } else {
        SnapshotCapacity::Capture
    }
}
