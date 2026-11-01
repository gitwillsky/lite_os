//! POSIX timer 的实际 schedule、预留通知与 dequeue 重装事务；不靠周期重复发送累积信号。
use super::{
    DeadlineEntry, ExpiredPosixTimer, ExpiredTimer, PosixTimerClock, PosixTimerNotification,
    PreparedPosixReplacement, RetiredTimers, TimerError, TimerIdentity, TimerQueue, TimerSetting,
    next_period,
};
use crate::execution::task::model::{PreallocatedSignal, TimerSignalDelivery};
use alloc::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum NotificationPhase {
    Publication,
    Delivery,
    Ignored,
}

/// deadline 的实际 membership：active index、暂停通知或 SIGEV_NONE lazy clock projection。
/// Notification 独占旧 AVL node，dequeue/重设可直接复用，不在信号路径分配。
pub(super) enum PosixSchedule {
    Disarmed,
    Armed {
        expiration: u64,
    },
    Passive {
        expiration: u64,
    },
    Notification {
        expiration: u64,
        deadline: DeadlineEntry,
        phase: NotificationPhase,
    },
}

pub(super) struct PosixTimer {
    pub(super) clock: PosixTimerClock,
    pub(super) notification: PosixTimerNotification,
    pub(super) schedule: PosixSchedule,
    pub(super) interval_ns: u64,
    pub(super) overrun: i32,
    pub(super) sequence: u64,
    pub(super) signal: Arc<PreallocatedSignal>,
    pub(super) thread_target: Option<Arc<crate::namespace::PidIdentity>>,
}
impl PosixTimer {
    fn snapshot(&self, now_ns: u64) -> TimerSetting {
        let remaining_ns = match &self.schedule {
            PosixSchedule::Disarmed => 0,
            PosixSchedule::Armed { expiration } => expiration.saturating_sub(now_ns).max(1),
            PosixSchedule::Notification { .. } if self.interval_ns == 0 => 0,
            PosixSchedule::Notification { expiration, .. }
            | PosixSchedule::Passive { expiration } => {
                let next = if self.interval_ns != 0 && *expiration <= now_ns {
                    next_period(*expiration, self.interval_ns, now_ns).0
                } else {
                    Some(*expiration)
                };
                next.map_or(0, |next| next.saturating_sub(now_ns))
            }
        };
        TimerSetting {
            remaining_ns,
            interval_ns: self.interval_ns,
        }
    }
    fn has_deadline_storage(&self) -> bool {
        matches!(
            self.schedule,
            PosixSchedule::Armed { .. } | PosixSchedule::Notification { .. }
        )
    }
    fn normalized_notification(&self, id: i32) -> PosixTimerNotification {
        match self.notification {
            PosixTimerNotification::Default => PosixTimerNotification::Process {
                signal: 14,
                value: id as u64,
            },
            notification => notification,
        }
    }
    fn expired(&self, key: (usize, i32)) -> ExpiredPosixTimer {
        ExpiredPosixTimer {
            tgid: key.0,
            id: key.1,
            notification: self.normalized_notification(key.1),
            sequence: self.sequence,
            owner: self.signal.clone(),
            thread_target: self.thread_target.clone(),
        }
    }
}

impl TimerQueue {
    pub(super) fn replace_posix(
        &mut self,
        prepared: &mut PreparedPosixReplacement,
        now_ns: u64,
        offsets: crate::namespace::TimeOffsets,
    ) -> Result<Option<(TimerSetting, RetiredTimers)>, TimerError> {
        let key = prepared.key;
        let current = self.posix_timers.get(&key).ok_or(TimerError::NotFound)?;
        let previous = current.snapshot(now_ns);
        let next =
            (prepared.value_ns != 0).then(|| {
                if !prepared.absolute {
                    now_ns
                        .saturating_add(prepared.value_ns)
                        .min(i64::MAX as u64)
                } else {
                    match current.clock {
                        PosixTimerClock::Realtime => {
                            crate::execution::timer::realtime_deadline_to_monotonic_ns(
                                prepared.value_ns,
                            )
                        }
                        PosixTimerClock::Monotonic => offsets
                            .to_host(crate::namespace::TimeClock::Monotonic, prepared.value_ns),
                        PosixTimerClock::Boottime => offsets
                            .to_host(crate::namespace::TimeClock::Boottime, prepared.value_ns),
                    }
                }
            });
        let active = current.notification != PosixTimerNotification::None;
        if next.is_some()
            && active
            && !current.has_deadline_storage()
            && prepared.deadline_node.is_none()
        {
            return Ok(None);
        }
        let identity = TimerIdentity::Posix(key.0, key.1);
        let timer = self.posix_timers.get_mut(&key).expect("actual POSIX timer");
        let old = core::mem::replace(&mut timer.schedule, PosixSchedule::Disarmed);
        let mut deadline = match old {
            PosixSchedule::Armed { expiration } => Some(
                self.deadline_index
                    .take_entry(&(expiration, identity))
                    .expect("active POSIX deadline"),
            ),
            PosixSchedule::Notification { deadline, .. } => Some(deadline),
            PosixSchedule::Disarmed | PosixSchedule::Passive { .. } => None,
        };
        timer.sequence = timer.sequence.wrapping_add(1);
        timer.interval_ns = if next.is_some() {
            prepared.interval_ns
        } else {
            0
        };
        timer.overrun = 0;
        if let Some(expiration) = next {
            if active {
                let mut entry = deadline
                    .take()
                    .or_else(|| prepared.deadline_node.take())
                    .expect("prepared POSIX deadline");
                entry.set_key((expiration, identity));
                self.deadline_index.commit_vacant(entry);
                timer.schedule = PosixSchedule::Armed { expiration };
            } else {
                timer.schedule = PosixSchedule::Passive { expiration };
            }
        }
        let mut retired = RetiredTimers::default();
        if let Some(deadline) = deadline {
            retired.deadlines.commit_vacant(deadline);
        }
        Ok(Some((previous, retired)))
    }

    pub(super) fn posix(
        &self,
        tgid: usize,
        id: i32,
        now_ns: u64,
    ) -> Result<TimerSetting, TimerError> {
        self.posix_timers
            .get(&(tgid, id))
            .map(|timer| timer.snapshot(now_ns))
            .ok_or(TimerError::NotFound)
    }
    pub(super) fn posix_deadline_needed(
        &self,
        tgid: usize,
        id: i32,
        replacement: bool,
    ) -> Result<bool, TimerError> {
        self.posix_timers
            .get(&(tgid, id))
            .map(|timer| {
                super::preparation_policy::posix_deadline_needed(
                    timer.has_deadline_storage(),
                    replacement && timer.notification != PosixTimerNotification::None,
                )
            })
            .ok_or(TimerError::NotFound)
    }
    pub(super) fn posix_overrun(&self, tgid: usize, id: i32) -> Result<i32, TimerError> {
        self.posix_timers
            .get(&(tgid, id))
            .map(|timer| timer.overrun)
            .ok_or(TimerError::NotFound)
    }
    pub(super) fn expire_posix(
        &mut self,
        key: (usize, i32),
        expiration: u64,
        deadline: DeadlineEntry,
    ) -> ExpiredTimer {
        let timer = self
            .posix_timers
            .get_mut(&key)
            .expect("POSIX deadline lost timer");
        assert!(
            matches!(timer.schedule, PosixSchedule::Armed { expiration: expected } if expected == expiration)
        );
        timer.schedule = PosixSchedule::Notification {
            expiration,
            deadline,
            phase: NotificationPhase::Publication,
        };
        ExpiredTimer::Posix(timer.expired(key))
    }
    /// Graph 冻结 process/namespace lifetime；验证真正的 timer Arc，拒绝同 ID 新 lifetime。
    pub(in crate::execution::task::process_table) fn prepare_posix_notification(
        &mut self,
        expired: &ExpiredPosixTimer,
    ) -> Option<crate::execution::task::model::PreparedSignalRecord> {
        let timer = self.posix_timers.get_mut(&(expired.tgid, expired.id))?;
        if !Arc::ptr_eq(&timer.signal, &expired.owner) || timer.sequence != expired.sequence {
            return None;
        }
        let PosixSchedule::Notification { phase, .. } = &mut timer.schedule else {
            return None;
        };
        if *phase != NotificationPhase::Publication {
            return None;
        }
        *phase = NotificationPhase::Delivery;
        Some(timer.signal.prepare(timer.sequence))
    }
    /// SIG_IGN 先改变 timer 的真实暂停状态，再由 caller 归还 Box；graph 保证 owner 不被删除。
    pub(in crate::execution::task::process_table) fn ignore_posix_notification(
        &mut self,
        event: &TimerSignalDelivery,
    ) -> bool {
        let Some(timer) = self.posix_timers.get_mut(&event.key) else {
            return false;
        };
        if !Arc::ptr_eq(&timer.signal, &event.owner)
            || timer.sequence != event.sequence
            || timer.interval_ns == 0
        {
            return false;
        }
        let PosixSchedule::Notification { phase, .. } = &mut timer.schedule else {
            return false;
        };
        *phase = NotificationPhase::Ignored;
        true
    }
    pub(in crate::execution::task::process_table) fn ignore_posix_expiry(
        &mut self,
        expired: &ExpiredPosixTimer,
    ) {
        let Some(timer) = self.posix_timers.get_mut(&(expired.tgid, expired.id)) else {
            return;
        };
        if !Arc::ptr_eq(&timer.signal, &expired.owner) || timer.sequence != expired.sequence {
            return;
        }
        if let PosixSchedule::Notification { phase, .. } = &mut timer.schedule {
            *phase = NotificationPhase::Ignored;
        }
    }
    /// 只遍历当前 process 的 timer 范围；每个被忽略周期 timer 的实际 schedule 仅迁移一次。
    pub(in crate::execution::task::process_table) fn resume_ignored_posix(
        &mut self,
        tgid: usize,
        signal: usize,
        after: i32,
    ) -> Option<ExpiredPosixTimer> {
        let mut cursor = (tgid, after);
        while let Some(key) = self
            .posix_timers
            .successor(&cursor)
            .map(|(&key, _)| key)
            .filter(|key| key.0 == tgid)
        {
            cursor = key;
            let timer = self.posix_timers.get_mut(&key).expect("actual POSIX timer");
            let number = match timer.normalized_notification(key.1) {
                PosixTimerNotification::Process { signal, .. }
                | PosixTimerNotification::Thread { signal, .. } => signal,
                PosixTimerNotification::Default | PosixTimerNotification::None => continue,
            };
            if number != signal || timer.interval_ns == 0 {
                continue;
            }
            if let PosixSchedule::Notification { phase, .. } = &mut timer.schedule
                && *phase == NotificationPhase::Ignored
            {
                *phase = NotificationPhase::Publication;
                return Some(timer.expired(key));
            }
        }
        None
    }
    fn deliver_posix_notification(
        &mut self,
        event: &TimerSignalDelivery,
        now_ns: u64,
        info: &mut crate::execution::task::PendingSignal,
    ) -> (bool, Option<DeadlineEntry>) {
        let Some(timer) = self.posix_timers.get_mut(&event.key) else {
            return (false, None);
        };
        if !Arc::ptr_eq(&timer.signal, &event.owner) || timer.sequence != event.sequence {
            return (false, None);
        }
        if !matches!(
            timer.schedule,
            PosixSchedule::Notification {
                phase: NotificationPhase::Delivery,
                ..
            }
        ) {
            return (false, None);
        }
        let PosixSchedule::Notification {
            expiration,
            mut deadline,
            ..
        } = core::mem::replace(&mut timer.schedule, PosixSchedule::Disarmed)
        else {
            unreachable!()
        };
        if timer.interval_ns == 0 {
            return (true, Some(deadline));
        }
        let (next, elapsed) = next_period(expiration, timer.interval_ns, now_ns);
        timer.overrun = elapsed.saturating_sub(1).min(i32::MAX as u64) as i32;
        info.set_timer_overrun(timer.overrun);
        timer.sequence = timer.sequence.wrapping_add(1);
        if let Some(expiration) = next {
            deadline.set_key((expiration, TimerIdentity::Posix(event.key.0, event.key.1)));
            self.deadline_index.commit_vacant(deadline);
            timer.schedule = PosixSchedule::Armed { expiration };
            (true, None)
        } else {
            (true, Some(deadline))
        }
    }
}

/// pending owner 已归还实际预留 Box 后，统一 sigwait/handler 消费路径验证并重装 timer。
/// 删除、重设或 ID 复用后返回 false，由 signal consumer 丢弃旧通知继续选择。
pub(in crate::execution::task) fn deliver_posix_signal(
    event: TimerSignalDelivery,
    info: &mut crate::execution::task::PendingSignal,
) -> bool {
    let (deliver, retired) = super::super::PROCESS_TABLE
        .timers
        .lock()
        .deliver_posix_notification(&event, crate::execution::timer::get_time_ns(), info);
    drop(retired);
    deliver
}
