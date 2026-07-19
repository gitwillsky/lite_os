use super::transaction_loop::{TimerTransactionCommit, run_timer_transaction};
use super::{PosixTimerNotification, TimerError, TimerQueue, live_process};

/// Timer mutation 的 lifecycle 与 target-specific validation policy。
#[derive(Clone, Copy)]
pub(super) enum TimerTransactionPolicy {
    /// Replace or disarm the process-owned ITIMER_REAL record.
    ItimerReal { tgid: usize },
    /// Create a POSIX timer, optionally requiring a live target thread.
    PosixCreate { tgid: usize, thread: Option<usize> },
    /// Replace or disarm an existing process-owned POSIX timer.
    PosixReplace { tgid: usize },
}

impl TimerTransactionPolicy {
    /// Construct the ITIMER_REAL replacement policy for `tgid`.
    pub(super) const fn itimer_real(tgid: usize) -> Self {
        Self::ItimerReal { tgid }
    }

    /// Construct POSIX creation policy and retain its thread-lifecycle requirement.
    pub(super) const fn posix_create(tgid: usize, notification: PosixTimerNotification) -> Self {
        let thread = match notification {
            PosixTimerNotification::Thread { tid, .. } => Some(tid),
            _ => None,
        };
        Self::PosixCreate { tgid, thread }
    }

    /// Construct the POSIX replacement policy for `tgid`.
    pub(super) const fn posix_replace(tgid: usize) -> Self {
        Self::PosixReplace { tgid }
    }

    fn validate(self, graph: &super::super::ProcessGraph) -> Result<(), TimerError> {
        let (tgid, thread) = match self {
            Self::ItimerReal { tgid } | Self::PosixReplace { tgid } => (tgid, None),
            Self::PosixCreate { tgid, thread } => (tgid, thread),
        };
        let threads = live_process(graph, tgid)?;
        if thread.is_some_and(|tid| !threads.contains_key(&tid)) {
            return Err(TimerError::NotFound);
        }
        Ok(())
    }

    fn initial_validate(self, graph: &super::super::ProcessGraph) -> Result<(), TimerError> {
        self.validate(graph)
    }
}

/// 在唯一 lock choreography 下执行 timer prepare/final-recheck/commit transaction。
///
/// `plan` 只在 timer owner lock 内读取 storage needs；`prepare` 在所有锁外分配；`commit`
/// 在 graph lifecycle guard 与 timer owner lock 同时持有时发布。失败或无复用价值的 retry
/// 会 Drop 唯一 prepared owner，不留下 record/deadline membership。
pub(super) fn execute_timer_transaction<Plan, Prepared, Output>(
    policy: TimerTransactionPolicy,
    mut plan: impl FnMut(&TimerQueue) -> Result<Plan, TimerError>,
    prepare: impl FnMut(Plan, Option<Prepared>) -> Result<Prepared, TimerError>,
    mut commit: impl FnMut(
        &mut TimerQueue,
        Prepared,
    ) -> Result<TimerTransactionCommit<Prepared, Output>, TimerError>,
) -> Result<Output, TimerError> {
    run_timer_transaction(
        || {
            let graph = super::super::TASK_MANAGER.graph.lock();
            policy.initial_validate(&graph)
        },
        || {
            let timers = super::super::TASK_MANAGER.timers.lock();
            plan(&timers)
        },
        prepare,
        |prepared| {
            let graph = super::super::TASK_MANAGER.graph.lock();
            policy.validate(&graph)?;
            let mut timers = super::super::TASK_MANAGER.timers.lock();
            commit(&mut timers, prepared)
        },
    )
}
