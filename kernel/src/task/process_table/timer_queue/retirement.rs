//! 真实 timer/deadline AVL node 的锁外退休；复用已摘 storage，不分配 cleanup snapshot。
use super::{KernelTimer, PosixTimer, RealTimer, TimerFile, TimerIdentity};
use crate::fallible_tree::FallibleMap;
use alloc::sync::Weak;

/// 唯一已摘除的 owned records；不保存 active mirror 或手动同步计数。
#[derive(Default)]
pub(in crate::task) struct RetiredTimers {
    pub(super) real: FallibleMap<usize, RealTimer>,
    pub(super) posix: FallibleMap<(usize, i32), PosixTimer>,
    pub(super) files: FallibleMap<u64, TimerFile>,
    pub(super) deadlines: FallibleMap<(u64, TimerIdentity), Option<Weak<KernelTimer>>>,
}

/// 固定 expiry 最多退休一个 deadline 与一个 dead timerfd record；避免把四棵空树带入 batch 栈。
pub(in crate::task) struct TimerExpirationRetirement {
    _deadline: Option<super::DeadlineEntry>,
    pub(super) file: Option<crate::fallible_tree::VacantEntry<u64, TimerFile>>,
}
impl TimerExpirationRetirement {
    pub(super) fn from_deadline(entry: Option<super::DeadlineEntry>) -> Self {
        Self {
            _deadline: entry,
            file: None,
        }
    }
}
