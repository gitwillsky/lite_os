//! 阻塞协议：同步原语（`TaskEvent`、`TaskMutex`、`WaitQueue`）经 `TaskWaitTarget` 让线程睡眠与被唤醒。
//!
//! 1. 等待者先 `begin_arming`，发布唯一等待身份并切换为 Blocking；
//! 2. 发布后复查：completion 已完成、线程已被 kill 或截止时间已过时立即自我唤醒；
//! 3. 唤醒者只能出示同一身份消费它，重复或过期的唤醒被忽略。
//!
//! 第 2 步覆盖“事件或 kill 发生在发布之前”的窗口，因此任何交错下都不会丢失唤醒。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use super::scheduling::WaitMembership;
use super::{Thread, context_switch, current, processor, timer_queue};
use crate::execution::sync::{TaskWaitKey, TaskWaitTarget, WaitCompletion, WaitResult};

// OWNER: 纯睡眠等待的唯一序号；Relaxed，只提供唯一性。
static NEXT_SLEEP: AtomicU64 = AtomicU64::new(1);

pub(super) fn install() {
    crate::execution::sync::install_wait_target_factory(|| {
        current().map(|thread| thread as Arc<dyn TaskWaitTarget>)
    });
}

/// kill 时打断可中断等待。
pub(super) fn interrupt(thread: &Arc<Thread>) {
    let interruptible = match processor::waiting_membership(thread) {
        Some(WaitMembership::Event(key)) => {
            key.interruptible().then_some(WaitMembership::Event(key))
        }
        Some(membership @ WaitMembership::Sleep(_)) => Some(membership),
        None => None,
    };
    if let Some(membership) = interruptible {
        processor::wake_waiting(thread.clone(), membership, WaitResult::Interrupted, false);
    }
}

/// 睡眠到截止时间。
///
/// # Returns
/// 到期返回 `TimedOut`；被 kill 返回 `Interrupted`；定时节点分配失败返回 `OutOfMemory`。
pub(super) fn sleep_until(deadline_ns: u64) -> WaitResult {
    let thread = current().expect("sleep requires a current thread");
    if thread.is_killed() {
        return WaitResult::Interrupted;
    }
    let membership = WaitMembership::Sleep(NEXT_SLEEP.fetch_add(1, Ordering::Relaxed));
    let Ok(timer) = timer_queue::prepare(deadline_ns, thread.clone(), membership) else {
        return WaitResult::OutOfMemory;
    };
    let prepared = context_switch::prepare_block(&thread, membership);
    let token = timer_queue::arm(timer);
    if thread.is_killed() {
        processor::wake_waiting(thread.clone(), membership, WaitResult::Interrupted, false);
    } else if deadline_ns <= crate::execution::timer::get_time_ns() {
        processor::wake_waiting(thread.clone(), membership, WaitResult::TimedOut, false);
    }
    let result = prepared.suspend();
    timer_queue::cancel(token);
    result
}

impl TaskWaitTarget for Thread {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) -> WaitResult {
        if !completion.begin_arming() {
            return WaitResult::Woken;
        }
        let membership = WaitMembership::Event(key);
        let timer = match key.deadline() {
            Some(deadline) => match timer_queue::prepare(deadline, self.clone(), membership) {
                Ok(timer) => Some((deadline, timer)),
                Err(()) => {
                    completion.finish_arming();
                    return WaitResult::OutOfMemory;
                }
            },
            None => None,
        };
        let prepared = context_switch::prepare_block(&self, membership);
        let token = timer.map(|(deadline, timer)| (deadline, timer_queue::arm(timer)));
        if completion.finish_arming() {
            processor::wake_waiting(self.clone(), membership, WaitResult::Woken, false);
        } else if key.signal_pending(self.as_ref()) {
            processor::wake_waiting(self.clone(), membership, WaitResult::Interrupted, false);
        } else if token
            .is_some_and(|(deadline, _)| deadline <= crate::execution::timer::get_time_ns())
        {
            processor::wake_waiting(self.clone(), membership, WaitResult::TimedOut, false);
        }
        let result = prepared.suspend();
        if let Some((_, token)) = token {
            timer_queue::cancel(token);
        }
        result
    }

    fn interrupted(&self) -> bool {
        self.is_killed()
    }

    fn wake(self: Arc<Self>, key: TaskWaitKey) {
        processor::wake_waiting(self, WaitMembership::Event(key), WaitResult::Woken, false);
    }

    fn wake_on_current_cpu(self: Arc<Self>, key: TaskWaitKey) {
        processor::wake_waiting(self, WaitMembership::Event(key), WaitResult::Woken, true);
    }
}
