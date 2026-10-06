//! 单等待者、自动复位的 task-context 事件。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{
    WaitCompletion,
    task_wait::{TaskWaitKey, TaskWaitTarget, current_wait_target},
};

/// 一个固定 owner（例如内核线程）等待、任意 task/IRQ 上下文 signal 的事件。
///
/// signal 先发布 `pending` 再取走已登记的 waiter；wait 先登记 waiter 再复查 `pending`。
/// 两侧按相反顺序访问这两个状态，因此任一交错下 signal 都不会丢失：要么 waiter 复查看到
/// `pending`，要么 signal 取到 waiter 并完成其 completion。
pub(crate) struct TaskEvent {
    // OWNER: 未被消费的 signal；缺失时 wait 登记前发生的 signal 会丢失并让 owner 永久阻塞。
    pending: AtomicBool,
    // OWNER: 当前唯一已登记 waiter；signal 只通过 take 取得，保证每次 wait 至多被唤醒一次。
    waiter: spin::Mutex<Option<(Arc<dyn TaskWaitTarget>, TaskWaitKey)>>,
    // OWNER: 唯一 waiter 的 arming 握手；事件只允许单等待者，因此可以内嵌而无需分配。
    completion: WaitCompletion,
    // OWNER: 区分同一事件上每次等待的 membership ticket；缺失时迟到的 wake 可能命中下一次等待。
    ticket: AtomicU64,
}

impl TaskEvent {
    pub(crate) const fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            waiter: spin::Mutex::new(None),
            completion: WaitCompletion::new(),
            ticket: AtomicU64::new(0),
        }
    }

    /// 阻塞当前 task，直到事件被 signal，并消费该 signal。
    ///
    /// # Panics
    ///
    /// 不在 task context 调用，或同一事件出现第二个并发 waiter 时 panic。
    pub(crate) fn wait(&self) {
        loop {
            if self.pending.swap(false, Ordering::AcqRel) {
                return;
            }
            let target = current_wait_target().expect("TaskEvent::wait requires task context");
            let key = TaskWaitKey {
                owner: self as *const Self as usize,
                ticket: self.ticket.fetch_add(1, Ordering::Relaxed),
            };
            self.completion.reset();
            {
                let mut waiter = self.waiter.lock();
                assert!(waiter.is_none(), "TaskEvent allows a single waiter");
                *waiter = Some((target.clone(), key));
            }
            // 登记后复查：signal 若在登记前发布了 pending 且未看到 waiter，由这里收回等待。
            if self.pending.load(Ordering::Acquire) && self.waiter.lock().take().is_some() {
                self.completion.complete();
                continue;
            }
            target.sleep(&self.completion, key);
        }
    }

    /// 发布一次 signal；waiter 已阻塞时将其唤醒。重复 signal 合并为一次。
    pub(crate) fn signal(&self) {
        self.pending.store(true, Ordering::Release);
        let Some((target, key)) = self.waiter.lock().take() else {
            return;
        };
        if self.completion.complete() {
            target.wake(key);
        }
    }
}
