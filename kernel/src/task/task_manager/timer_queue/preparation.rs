use super::preparation_policy::TimerReplacementNeeds;
use super::{
    PosixTimer, PosixTimerClock, PosixTimerNotification, RealTimer, TimerError, TimerIdentity,
};
use crate::fallible_tree::{FallibleMap, VacantEntry};

/// @description 锁外完成 ITIMER_REAL record/deadline node 分配的替换事务。
/// @ownership 未提交节点只归本对象；Process 复查失败或已有 node 被复用时由 Drop 回收。
pub(super) struct PreparedRealReplacement {
    pub(super) tgid: usize,
    pub(super) replacement: Option<RealTimer>,
    pub(super) timer_node: Option<VacantEntry<usize, RealTimer>>,
    pub(super) deadline_node: Option<VacantEntry<(u64, TimerIdentity), ()>>,
}

impl PreparedRealReplacement {
    pub(super) fn prepare(
        tgid: usize,
        value_ns: u64,
        interval_ns: u64,
        now_ns: u64,
        needs: TimerReplacementNeeds,
    ) -> Result<Self, TimerError> {
        let next = (value_ns != 0).then(|| now_ns.saturating_add(value_ns));
        let replacement = (next.is_some() || interval_ns != 0).then_some(RealTimer {
            next_expiration_ns: next,
            interval_ns,
        });
        let timer_node = (if needs.record { replacement } else { None })
            .map(|timer| FallibleMap::try_prepare(tgid, timer))
            .transpose()
            .map_err(|_| TimerError::OutOfMemory)?;
        let deadline_node = (if needs.deadline { next } else { None })
            .map(|_| FallibleMap::try_prepare((0, TimerIdentity::Real(tgid)), ()))
            .transpose()
            .map_err(|_| TimerError::OutOfMemory)?;
        Ok(Self {
            tgid,
            replacement,
            timer_node,
            deadline_node,
        })
    }
}

/// @description 锁外完成 POSIX timer record node 分配的创建事务。
/// @ownership ID 冲突或 Process 生命周期复查失败时，唯一 node 随本对象回收且未发布。
pub(super) struct PreparedPosixCreate {
    pub(super) key: (usize, i32),
    pub(super) timer_node: VacantEntry<(usize, i32), PosixTimer>,
}

impl PreparedPosixCreate {
    pub(super) fn prepare(
        tgid: usize,
        id: i32,
        clock: PosixTimerClock,
        notification: PosixTimerNotification,
    ) -> Result<Self, TimerError> {
        let key = (tgid, id);
        let timer_node = FallibleMap::try_prepare(
            key,
            PosixTimer {
                clock,
                notification,
                next_expiration_ns: None,
                interval_ns: 0,
                overrun: 0,
            },
        )
        .map_err(|_| TimerError::OutOfMemory)?;
        Ok(Self { key, timer_node })
    }

    /// @description ID 冲突后复用同一未发布 record node，不重新分配。
    /// @param id timer owner 重新选择的未占用 candidate。
    /// @return key 与 VacantEntry key 同步更新后的原 transaction。
    pub(super) fn retarget(mut self, id: i32) -> Self {
        self.key.1 = id;
        self.timer_node.set_key(self.key);
        self
    }
}

/// @description 锁外完成 POSIX timer 可选 deadline node 分配的替换事务。
/// @ownership timer 删除/已有 deadline 被复用时，未提交 placeholder node 由 Drop 回收。
pub(super) struct PreparedPosixReplacement {
    pub(super) key: (usize, i32),
    pub(super) value_ns: u64,
    pub(super) interval_ns: u64,
    pub(super) absolute: bool,
    pub(super) deadline_node: Option<VacantEntry<(u64, TimerIdentity), ()>>,
}

impl PreparedPosixReplacement {
    pub(super) fn prepare(
        tgid: usize,
        id: i32,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        deadline_needed: bool,
    ) -> Result<Self, TimerError> {
        let key = (tgid, id);
        let deadline_node = (value_ns != 0 && deadline_needed)
            .then(|| FallibleMap::try_prepare((0, TimerIdentity::Posix(tgid, id)), ()))
            .transpose()
            .map_err(|_| TimerError::OutOfMemory)?;
        Ok(Self {
            key,
            value_ns,
            interval_ns,
            absolute,
            deadline_node,
        })
    }
}
