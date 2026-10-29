use super::transaction_loop::{TimerTransactionCommit, run_timer_transaction};
use super::{TimerError, TimerQueue, live_process};

/// Timer mutation 的 lifecycle 与 target-specific validation policy。
pub(super) enum TimerTransactionPolicy {
    /// Replace or disarm the process-owned ITIMER_REAL record.
    ItimerReal { tgid: usize },
    /// Create a POSIX timer, optionally requiring a live target thread.
    PosixCreate {
        tgid: usize,
        thread: Option<alloc::sync::Arc<crate::namespace::PidIdentity>>,
    },
    /// Replace or disarm an existing process-owned POSIX timer.
    PosixReplace { tgid: usize },
}

impl TimerTransactionPolicy {
    /// Construct the ITIMER_REAL replacement policy for `tgid`.
    pub(super) const fn itimer_real(tgid: usize) -> Self {
        Self::ItimerReal { tgid }
    }

    /// Construct POSIX creation policy and retain its thread-lifecycle requirement.
    pub(super) fn posix_create(
        tgid: usize,
        thread: Option<alloc::sync::Arc<crate::namespace::PidIdentity>>,
    ) -> Self {
        Self::PosixCreate { tgid, thread }
    }

    /// Construct the POSIX replacement policy for `tgid`.
    pub(super) const fn posix_replace(tgid: usize) -> Self {
        Self::PosixReplace { tgid }
    }

    fn validate(&self, graph: &super::super::ProcessGraph) -> Result<(), TimerError> {
        let (tgid, thread) = match self {
            Self::ItimerReal { tgid } | Self::PosixReplace { tgid } => (tgid, None),
            Self::PosixCreate { tgid, thread } => (tgid, thread.as_ref()),
        };
        let threads = live_process(graph, *tgid)?;
        if thread.is_some_and(|expected| {
            threads
                .get(&expected.key())
                .is_none_or(|actual| !alloc::sync::Arc::ptr_eq(expected, &actual.tid_identity()))
        }) {
            return Err(TimerError::NotFound);
        }
        Ok(())
    }

    fn initial_validate(&self, graph: &super::super::ProcessGraph) -> Result<(), TimerError> {
        self.validate(graph)
    }
}

/// 在唯一 lock choreography 下执行 timer prepare/final-recheck/commit transaction。
///
/// `plan` 只在 timer owner lock 内读取 storage needs；`prepare` 在所有锁外分配；`commit`
/// 在 graph lifecycle guard 与 timer owner lock 同时持有时只借用 prepared 发布。
/// guard 释放后才消费未使用 storage；stale plan 复用同一未发布 node，失败只在锁外 Drop。
pub(super) fn execute_timer_transaction<Plan, Prepared, Output>(
    policy: TimerTransactionPolicy,
    mut plan: impl FnMut(&TimerQueue) -> Result<Plan, TimerError>,
    prepare: impl FnMut(Plan, Option<Prepared>) -> Result<Prepared, TimerError>,
    mut commit: impl FnMut(&mut TimerQueue, &mut Prepared) -> Result<Option<Output>, TimerError>,
) -> Result<Output, TimerError> {
    run_timer_transaction(
        || {
            let graph = super::super::PROCESS_TABLE.graph.lock();
            policy.initial_validate(&graph)
        },
        || {
            let timers = super::super::PROCESS_TABLE.timers.lock();
            plan(&timers)
        },
        prepare,
        |mut prepared| {
            // prepared 始终归外层栈帧；final owner 只借用。成功/失败/重试都不能在 guard
            // 内 Drop 未使用的 reserved node，尤其是计划过期后已不需要的 placeholder。
            let outcome = {
                let graph = super::super::PROCESS_TABLE.graph.lock();
                policy.validate(&graph)?;
                let mut timers = super::super::PROCESS_TABLE.timers.lock();
                commit(&mut timers, &mut prepared)
            }?;
            Ok(match outcome {
                Some(output) => TimerTransactionCommit::Complete(output),
                None => TimerTransactionCommit::Retry(Some(prepared)),
            })
        },
    )
}
