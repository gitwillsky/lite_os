//! TASK_KILLABLE acquisition；沿既有 FIFO queue 取消，不另设锁/等待 owner。
use super::*;
use crate::execution::sync::task_wait::TaskWaitMode;

/// killable mutex acquisition 在进入 protected operation 前的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskMutexLockError {
    OutOfMemory,
    Interrupted,
}
impl<T: ?Sized> TaskMutex<T> {
    /// 阻塞取得 guard；普通信号不取消，SIGKILL 可取消 queue/handoff。
    /// 取消发生在 operation 前，未消费保护的状态；已选 handoff 必须先取得并释放 owner。
    pub(crate) fn lock_killable(&self) -> Result<TaskMutexGuard<'_, T>, TaskMutexLockError> {
        self.lock_with_signals(TaskWaitMode::Killable)
    }
    /// 可回滚 preparation acquisition；任意可交付 signal 可取消。
    pub(crate) fn lock_interruptible(&self) -> Result<TaskMutexGuard<'_, T>, TaskMutexLockError> {
        self.lock_with_signals(TaskWaitMode::Interruptible)
    }
    fn lock_with_signals(
        &self,
        mode: TaskWaitMode,
    ) -> Result<TaskMutexGuard<'_, T>, TaskMutexLockError> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        let mut preparation =
            TaskMutexWaitPreparation::prepare().map_err(|_| TaskMutexLockError::OutOfMemory)?;
        let waiter = preparation.arm_mode(self, mode);
        {
            let mut state = self.state.lock();
            if state.ownership == Ownership::Available {
                state.ownership = Ownership::Held;
                return Ok(TaskMutexGuard::new(self));
            }
            state.push(waiter.clone());
        }
        let result = waiter
            .target
            .as_ref()
            .cloned()
            .expect("killable mutex target")
            .sleep(waiter.completion.as_ref(), waiter.key);
        let mut state = self.state.lock();
        if result == super::super::WaitResult::Woken {
            assert_eq!(state.ownership, Ownership::Handoff(waiter.key.ticket));
            state.ownership = Ownership::Held;
            drop(state);
            return Ok(TaskMutexGuard::new(self));
        }
        assert_eq!(result, super::super::WaitResult::Interrupted);
        let handed_off = state.ownership == Ownership::Handoff(waiter.key.ticket);
        let retired = if handed_off {
            state.ownership = Ownership::Held;
            None
        } else if state
            .head
            .as_ref()
            .is_some_and(|head| head.key == waiter.key)
        {
            state.pop()
        } else {
            let mut previous = state
                .head
                .as_ref()
                .cloned()
                .expect("cancelled mutex waiter is queued");
            loop {
                let next = previous
                    .next
                    .lock()
                    .as_ref()
                    .cloned()
                    .expect("cancelled mutex waiter is queued");
                if next.key == waiter.key {
                    let removed = previous.next.lock().take().expect("queued next");
                    *previous.next.lock() = removed.next.lock().take();
                    if state
                        .tail
                        .as_ref()
                        .is_some_and(|tail| tail.key == waiter.key)
                    {
                        state.tail = Some(previous);
                    }
                    break Some(removed);
                }
                previous = next;
            }
        };
        drop(state);
        drop(retired);
        // 一次性 waiter 不重用。unlock 可已提取 publisher 但尚未 complete；不依赖 Arc 唯一性。
        // late wake 的 killable key 合法失败，不能把失败 cancellation 当成下一次 handoff。
        waiter.completion.complete();
        if handed_off {
            drop(TaskMutexGuard::new(self));
        }
        Err(TaskMutexLockError::Interrupted)
    }
}
