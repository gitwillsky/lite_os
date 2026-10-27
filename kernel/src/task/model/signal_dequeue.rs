//! private/shared pending 的唯一消费 owner；失效 timer 的退休不终止 dequeue。
use super::*;

/// 被消费事件的原 pending owner；ptrace 改成 blocked 信号时必须归还同一队列。
#[derive(Clone, Copy)]
pub(in crate::task) enum SignalQueue {
    Thread,
    Process,
}

impl TaskControlBlock {
    /// 原子检查给定 signal set 是否含 pending signal，并在成立时执行短操作。
    ///
    /// # Parameters
    ///
    /// - `mask`: `rt_sigtimedwait` 正在等待的 signal set。
    /// - `action`: 与统一 wait owner lock 配合的非阻塞操作。
    ///
    /// # Returns
    ///
    /// set 中有 pending signal 时返回操作结果，否则返回 None。
    pub(in crate::task) fn with_pending_signal<T>(
        &self,
        mask: u64,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        if self.is_kernel_thread() {
            return None;
        }
        let state = self.process().signal_state.lock();
        let pending = self.thread().pending_signals.lock();
        ((pending.bits | state.pending.bits) & mask != 0).then(action)
    }

    /// 先消费 thread-private，再消费 shared pending；各 owner 中按号选择、同号实时 FIFO。
    ///
    /// # Parameters
    ///
    /// - `mask`: 待消费的 signal set。
    ///
    /// # Returns
    ///
    /// signal number、首个 siginfo 与原队列；没有匹配时返回 None。
    pub(in crate::task) fn take_pending_signal(
        &self,
        mask: u64,
    ) -> Option<(usize, PendingSignal, SignalQueue)> {
        if self.is_kernel_thread() {
            return None;
        }
        loop {
            let mut state = self.process().signal_state.lock();
            let mut pending = self.thread().pending_signals.lock();
            let selected = pending
                .take(mask)
                .map(|event| (event, SignalQueue::Thread))
                .or_else(|| {
                    state
                        .pending
                        .take(mask)
                        .map(|event| (event, SignalQueue::Process))
                });
            drop(pending);
            drop(state);
            let (selected, queue) = selected?;
            if let Some(delivery) =
                selected.resolve(super::super::process_table::timer_queue::deliver_posix_signal)
            {
                return Some((delivery.0, delivery.1, queue));
            }
            // 已删除或被 settime 替换的 timer record 不是可见事件；继续消费真实队列，
            // 否则同号后续通知仍存在时 sigwait/signalfd 会错误返回 EAGAIN 或重新阻塞。
        }
    }

    /// 标准 rt_sigpending 的唯一瞬时投影；只返回当前 Thread blocked 的 private/shared union。
    pub(crate) fn pending_signal_set(&self) -> u64 {
        if self.is_kernel_thread() {
            return 0;
        }
        let mask = self.thread().signal_mask.lock();
        let shared = self.process().signal_state.lock();
        let private = self.thread().pending_signals.lock();
        (private.bits | shared.pending.bits) & *mask
    }
}

impl TaskControlBlock {
    /// 查询当前 Thread 是否有未屏蔽 pending signal。
    ///
    /// # Returns
    ///
    /// 至少一个 signal 可在 trap return 交付时返回 true。
    pub(in crate::task) fn has_deliverable_signal(&self) -> bool {
        self.with_deliverable_signal(|| ()).is_some()
    }

    /// 持有 mask/pending 锁复查 signal，并在其仍可交付时执行一次操作。
    ///
    /// # Parameters
    ///
    /// - `action`: 必须与 wait owner lock 配合的短临界区，不得阻塞或调度。
    ///
    /// # Returns
    ///
    /// signal 仍可交付时返回 action 结果，否则返回 None。
    pub(in crate::task) fn with_deliverable_signal<T>(
        &self,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        self.with_deliverable_signal_policy(false, action)
    }
    pub(in crate::task) fn has_fatal_signal(&self) -> bool {
        self.with_deliverable_signal_policy(true, || ()).is_some()
    }
    pub(in crate::task) fn with_deliverable_signal_policy<T>(
        &self,
        fatal_only: bool,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        if self.is_kernel_thread() {
            return None;
        }
        let mask = self.thread().signal_mask.lock();
        self.process()
            .signal_state
            .lock()
            .with_handlers(|handlers, shared_pending| {
                let pending = self.thread().pending_signals.lock();
                let available = (pending.bits | shared_pending.bits) & !*mask;
                let available = if fatal_only {
                    available & (1u64 << (syscall_abi::signal::SIGKILL - 1))
                } else {
                    available
                };
                (handlers.caught_set(available) != 0
                    || pending.requires_delivery(available)
                    || shared_pending.requires_delivery(available))
                .then(action)
            })
            .flatten()
    }
}
