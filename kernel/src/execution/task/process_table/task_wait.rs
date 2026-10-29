use alloc::sync::Arc;

use super::{TaskControlBlock, WaitMembership, WaitResult};
use crate::execution::sync::{TaskWaitKey, TaskWaitTarget, WaitCompletion};

pub(in crate::execution::task) fn initialize() {
    crate::execution::sync::install_wait_target_factory(current_wait_target);
}

fn current_wait_target() -> Option<Arc<dyn TaskWaitTarget>> {
    crate::execution::task::current_task().map(|task| task as Arc<dyn TaskWaitTarget>)
}

impl TaskWaitTarget for TaskControlBlock {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) -> WaitResult {
        if !completion.begin_arming() {
            return WaitResult::Woken;
        }
        if let Some(deadline) = key.deadline() {
            let ticket = super::WAIT_REGISTRY.allocate_ticket();
            let prepared = ticket.prepare_task_event(key, self.clone());
            let prepared = match super::arm_indexed_wait(
                &self,
                prepared,
                || {
                    if completion.is_complete() {
                        Some(WaitResult::Woken)
                    } else if deadline <= crate::execution::timer::get_time_ns() {
                        Some(WaitResult::TimedOut)
                    } else if key.signal_pending(self.as_ref()) {
                        Some(WaitResult::Interrupted)
                    } else {
                        None
                    }
                },
                |_| WaitMembership::TaskWait(key),
            ) {
                Ok(prepared) => prepared,
                Err(result) => return result,
            };
            if completion.finish_arming() {
                wake_timed_task_event(&self, key, WaitResult::Woken, false);
            } else if key.signal_pending(self.as_ref()) {
                wake_timed_task_event(&self, key, WaitResult::Interrupted, false);
            }
            return prepared.suspend();
        }
        let prepared = super::context_switch::prepare_current_block(&self, (), |_, _| {
            WaitMembership::TaskWait(key)
        });
        if completion.finish_arming() {
            crate::execution::task::processor::wake_waiting_task(
                self.clone(),
                WaitMembership::TaskWait(key),
                Some(WaitResult::Woken),
            );
        } else if key.signal_pending(self.as_ref()) {
            // membership 已发布后复查，覆盖信号先于 publication 到达的窗口。
            // event completion 与 signal 仅通过同一精确 membership 竞争一次 wake。
            crate::execution::task::processor::wake_waiting_task(
                self.clone(),
                WaitMembership::TaskWait(key),
                Some(WaitResult::Interrupted),
            );
        }
        prepared.suspend()
    }

    fn fatal_interrupted(&self) -> bool {
        self.has_fatal_signal()
    }
    fn interrupted(&self) -> bool {
        self.has_deliverable_signal()
    }

    fn wake_on_current_cpu(self: Arc<Self>, key: TaskWaitKey) {
        if key.deadline().is_some() {
            wake_timed_task_event(&self, key, WaitResult::Woken, true);
            return;
        }
        let woken = crate::execution::task::processor::wake_waiting_task_on_current_cpu(
            self,
            WaitMembership::TaskWait(key),
            Some(WaitResult::Woken),
        );
        assert!(
            woken || key.interruptible(),
            "stale noninterruptible task wake"
        );
    }

    fn wake(self: Arc<Self>, key: TaskWaitKey) {
        if key.deadline().is_some() {
            wake_timed_task_event(&self, key, WaitResult::Woken, false);
            return;
        }
        // Interrupted 可先消费 membership；迟到的 event wake 不能命中下一 ticket。
        let woken = crate::execution::task::processor::wake_waiting_task(
            self,
            WaitMembership::TaskWait(key),
            Some(WaitResult::Woken),
        );
        assert!(
            woken || key.interruptible(),
            "stale noninterruptible task wake"
        );
    }
}

// expiry/event/signal 共用一个 actual registration claim；任何迟到 wake 都不能留下 deadline 索引。
pub(super) fn wake_timed_task_event(
    task: &Arc<TaskControlBlock>,
    key: TaskWaitKey,
    result: WaitResult,
    current_cpu: bool,
) -> bool {
    let Some(wake) = super::WAIT_REGISTRY.wake_task_event(key, result) else {
        return false;
    };
    let Some(claimed) = wake.claimed else {
        return true;
    };
    assert!(Arc::ptr_eq(task, &claimed.task));
    if current_cpu {
        crate::execution::task::processor::wake_waiting_task_on_current_cpu(
            claimed.task,
            WaitMembership::TaskWait(key),
            Some(result),
        )
    } else {
        crate::execution::task::processor::wake_waiting_task(
            claimed.task,
            WaitMembership::TaskWait(key),
            Some(result),
        )
    }
}
