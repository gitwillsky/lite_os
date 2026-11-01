//! 信号来源通知与精确等待的中断；只消费等待 owner，不生成信号。
use super::*;

/// signal publication 或 signalfd mask change 后，在 Graph/pending guard 外唤醒真实 sighand 源。
pub(crate) fn notify_signal_waiters(task: &TaskControlBlock) {
    use crate::file::FileReadinessContext;
    let Some(source) = task.signal_source() else {
        return;
    };
    let notification = WAIT_REGISTRY.notification();
    source.changed();
    crate::file::Epoll::notify_signal_source(&source);
    while let Some(wake) = notification.wake_signal_source_one(source.identity()) {
        if let Some(claimed) = wake.claimed {
            assert!(matches!(claimed.kind, IndexedWaitKind::Poll));
            let _ =
                crate::task::processor::wake_poll_task(claimed.task, claimed.id, WaitResult::Woken);
        }
    }
}

/// 从当前唯一 wait owner 取消目标 task 的 interruptible wait。
pub(in crate::task::process_table) fn interrupt_waiting_task(task: &Arc<TaskControlBlock>) -> bool {
    let key = match task.scheduling.state.lock().wait {
        Some(WaitMembership::TaskWait(key)) if key.interruptible() => Some(key),
        _ => None,
    };
    let event_wait = key.and_then(|key| {
        task.with_deliverable_signal_policy(key.killable(), || {
            (task.scheduling.state.lock().wait == Some(WaitMembership::TaskWait(key)))
                .then_some(key)
        })
        .flatten()
    });
    if let Some(key) = event_wait {
        if key.deadline().is_some() {
            return super::super::task_wait::wake_timed_task_event(
                task,
                key,
                WaitResult::Interrupted,
                false,
            );
        }
        return crate::task::processor::wake_waiting_task(
            task.clone(),
            WaitMembership::TaskWait(key),
            Some(WaitResult::Interrupted),
        );
    }
    if task.has_deliverable_signal()
        && let Some(wake) = WAIT_REGISTRY.interrupt_task(task)
    {
        let Some(entry) = wake.claimed else {
            return true;
        };
        let wait_id = entry.id;
        let interrupted = WaitResult::Interrupted;
        assert!(Arc::ptr_eq(&entry.task, task));
        return match entry.kind {
            IndexedWaitKind::TaskEvent(key) => crate::task::processor::wake_waiting_task(
                entry.task,
                WaitMembership::TaskWait(key),
                Some(interrupted),
            ),
            IndexedWaitKind::Deadline => crate::task::processor::wake_deadline_task(
                entry.task,
                wait_id,
                WaitResult::Interrupted,
            ),
            IndexedWaitKind::Futex { .. } => crate::task::processor::wake_futex_task(
                entry.task,
                wait_id,
                WaitResult::Interrupted,
            ),
            IndexedWaitKind::Console => {
                crate::task::processor::wake_console_task(entry.task, wait_id, interrupted)
            }
            IndexedWaitKind::Signal { .. } => crate::task::processor::wake_signal_task(
                entry.task,
                wait_id,
                WaitResult::Interrupted,
            ),
            IndexedWaitKind::Pipe { .. } => {
                crate::task::processor::wake_pipe_task(entry.task, wait_id, WaitResult::Interrupted)
            }
            IndexedWaitKind::AdvisoryLock => {
                crate::task::processor::wake_flock_task(entry.task, wait_id, interrupted)
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Interrupted)
            }
        };
    }

    let child = {
        let mut graph = PROCESS_TABLE.graph.lock();
        task.with_deliverable_signal(|| {
            let scheduling = task.scheduling.state.lock();
            if scheduling.wait != Some(WaitMembership::Child) {
                None
            } else {
                let waiter = graph
                    .nodes
                    .get_mut(&task.tgid())
                    .expect("waiting process disappeared from graph")
                    .child_waiters
                    .remove(&task.tid());
                if let Some(waiter) = &waiter {
                    assert!(Arc::ptr_eq(waiter, task));
                }
                waiter
            }
        })
        .flatten()
    };
    child.is_some_and(|waiter| {
        crate::task::processor::wake_child_task(waiter, WaitResult::Interrupted)
    })
}
