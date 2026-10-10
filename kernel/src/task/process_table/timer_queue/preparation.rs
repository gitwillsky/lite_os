use super::preparation_policy::TimerReplacementNeeds;
use super::{
    PosixTimer, PosixTimerClock, PosixTimerNotification, RealTimer, TimerError, TimerIdentity,
};
use crate::fallible_tree::{FallibleMap, VacantEntry};

/// 锁外完成 ITIMER_REAL record/deadline node 分配的替换事务。
///
/// 未提交节点只归本对象；Process 复查失败或已有 node 被复用时由 Drop 回收。
pub(super) struct PreparedRealReplacement {
    pub(super) tgid: usize,
    pub(super) replacement: Option<RealTimer>,
    pub(super) timer_node: Option<VacantEntry<usize, RealTimer>>,
    pub(super) deadline_node: Option<super::DeadlineEntry>,
}

impl PreparedRealReplacement {
    pub(super) fn prepare(
        tgid: usize,
        value_ns: u64,
        interval_ns: u64,
        now_ns: u64,
        needs: TimerReplacementNeeds,
        reusable: Option<Self>,
    ) -> Result<Self, TimerError> {
        let next = (value_ns != 0).then(|| now_ns.saturating_add(value_ns).min(i64::MAX as u64));
        let replacement = (next.is_some() || interval_ns != 0).then_some(RealTimer {
            next_expiration_ns: next,
            interval_ns,
        });
        let mut prepared = reusable.unwrap_or(Self {
            tgid,
            replacement,
            timer_node: None,
            deadline_node: None,
        });
        if needs.record
            && prepared.timer_node.is_none()
            && let Some(timer) = replacement
        {
            prepared.timer_node =
                Some(FallibleMap::try_prepare(tgid, timer).map_err(|_| TimerError::OutOfMemory)?);
        }
        if needs.deadline && next.is_some() && prepared.deadline_node.is_none() {
            prepared.deadline_node = Some(
                FallibleMap::try_prepare((0, TimerIdentity::Real(tgid)), None)
                    .map_err(|_| TimerError::OutOfMemory)?,
            );
        }
        Ok(prepared)
    }
}

/// 锁外完成 POSIX timer record node 分配的创建事务。
///
/// ID 冲突或 Process 生命周期复查失败时，唯一 node 随本对象回收且未发布。
pub(super) struct PreparedPosixCreate {
    pub(super) key: (usize, i32),
    pub(super) timer_node: Option<VacantEntry<(usize, i32), PosixTimer>>,
}

impl PreparedPosixCreate {
    pub(super) fn prepare(
        tgid: usize,
        id: i32,
        clock: PosixTimerClock,
        notification: PosixTimerNotification,
        thread_target: Option<alloc::sync::Arc<crate::namespace::PidIdentity>>,
    ) -> Result<Self, TimerError> {
        let key = (tgid, id);
        let timer_node = FallibleMap::try_prepare(
            key,
            PosixTimer {
                clock,
                notification,
                thread_target,
                schedule: super::posix_schedule::PosixSchedule::Disarmed,
                interval_ns: 0,
                overrun: 0,
                sequence: 0,
                signal: {
                    let creator = crate::task::current_task().ok_or(TimerError::NotFound)?;
                    let (namespace, uid) = creator.user_resource_owner();
                    crate::task::model::PreallocatedSignal::try_new(
                        namespace,
                        uid,
                        creator
                            .resource_limit(crate::task::model::RLIMIT_SIGPENDING)
                            .expect("RLIMIT_SIGPENDING exists")
                            .soft,
                        key,
                    )
                    .map_err(|_| TimerError::Exhausted)?
                },
            },
        )
        .map_err(|_| TimerError::Exhausted)?;
        Ok(Self {
            key,
            timer_node: Some(timer_node),
        })
    }

    /// ID 冲突后复用同一未发布 record node，不重新分配。
    ///
    /// # Parameters
    ///
    /// - `id`: timer owner 重新选择的未占用 candidate。
    ///
    /// # Returns
    ///
    /// key 与 VacantEntry key 同步更新后的原 transaction。
    pub(super) fn retarget(mut self, id: i32) -> Self {
        self.key.1 = id;
        let node = self.timer_node.as_mut().expect("unpublished POSIX node");
        node.value().signal.retarget(self.key);
        node.set_key(self.key);
        self
    }
}

/// 锁外完成 POSIX timer 可选 deadline node 分配的替换事务。
///
/// timer 删除/已有 deadline 被复用时，未提交 placeholder node 由 Drop 回收。
pub(super) struct PreparedPosixReplacement {
    pub(super) key: (usize, i32),
    pub(super) value_ns: u64,
    pub(super) interval_ns: u64,
    pub(super) absolute: bool,
    pub(super) deadline_node: Option<super::DeadlineEntry>,
}

impl PreparedPosixReplacement {
    pub(super) fn prepare(
        tgid: usize,
        id: i32,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        deadline_needed: bool,
        reusable: Option<Self>,
    ) -> Result<Self, TimerError> {
        let key = (tgid, id);
        let mut prepared = reusable.unwrap_or(Self {
            key,
            value_ns,
            interval_ns,
            absolute,
            deadline_node: None,
        });
        if value_ns != 0 && deadline_needed && prepared.deadline_node.is_none() {
            prepared.deadline_node = Some(
                FallibleMap::try_prepare((0, TimerIdentity::Posix(tgid, id)), None)
                    .map_err(|_| TimerError::OutOfMemory)?,
            );
        }
        Ok(prepared)
    }
}
