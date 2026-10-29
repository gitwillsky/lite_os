//! 单等待者、自动复位的 task-context 事件。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::{
    WaitCompletion, WaitResult,
    task_wait::{TaskWaitKey, TaskWaitMode, TaskWaitTarget, current_wait_target},
};

// OWNER: event cancellation 可以比已提取的 wake 先返回，旧 event 随后可能在同地址重建。
// 全局不复用 ticket 封闭跨对象生命周期 ABA；只在阻塞登记时领取，不分配、不承载内容 ordering。
static EVENT_TICKETS: AtomicU64 = AtomicU64::new(0);

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
}

impl TaskEvent {
    pub(crate) const fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            waiter: spin::Mutex::new(None),
            completion: WaitCompletion::new(),
        }
    }

    /// 阻塞当前 task，直到事件被 signal，并消费该 signal。
    ///
    /// # Panics
    ///
    /// 不在 task context 调用，或同一事件出现第二个并发 waiter 时 panic。
    pub(crate) fn wait(&self) {
        assert_eq!(
            self.wait_mode(TaskWaitMode::Uninterruptible, None),
            WaitResult::Woken
        );
    }

    /// 无待处理工作时的 kernel daemon 空闲等待；保持不可中断 handshake，但不计入 load。
    pub(crate) fn wait_idle(&self) {
        assert_eq!(self.wait_mode(TaskWaitMode::Idle, None), WaitResult::Woken);
    }

    /// 不可中断的有界设备事务等待；超时交由设备 owner 停止或隔离 DMA。
    ///
    /// # Parameters
    /// - `deadline_ns`: monotonic 绝对截止时间。
    /// # Returns
    /// signal 返回 Woken，到期返回 TimedOut，wait membership 分配失败返回 OutOfMemory。
    /// 普通信号不取消已提交给硬件的事务。
    pub(crate) fn wait_until(&self, deadline_ns: u64) -> WaitResult {
        self.wait_mode(TaskWaitMode::Uninterruptible, Some(deadline_ns))
    }

    /// 等待事件或可交付信号；不分配 waiter，Interrupted 后事件及 owner 继续有效。
    pub(crate) fn wait_interruptible(&self) -> WaitResult {
        self.wait_mode(TaskWaitMode::Interruptible, None)
    }

    /// PONR cleanup 仅允许致命 signal 取消；普通捕获信号继续等待。
    pub(crate) fn wait_killable(&self) -> WaitResult {
        self.wait_mode(TaskWaitMode::Killable, None)
    }

    /// 复用实际 event/arming owner 与 scheduler deadline index；到期或 OOM 取消时不消费未来 event。
    pub(crate) fn wait_interruptible_until(&self, deadline_ns: u64) -> WaitResult {
        self.wait_mode(TaskWaitMode::Interruptible, Some(deadline_ns))
    }
    fn wait_mode(&self, mode: TaskWaitMode, deadline_ns: Option<u64>) -> WaitResult {
        loop {
            if self.pending.swap(false, Ordering::AcqRel) {
                return WaitResult::Woken;
            }
            let target = current_wait_target().expect("TaskEvent::wait requires task context");
            if match mode {
                TaskWaitMode::Uninterruptible | TaskWaitMode::Idle => false,
                TaskWaitMode::Interruptible => target.interrupted(),
                TaskWaitMode::Killable => target.fatal_interrupted(),
            } {
                return WaitResult::Interrupted;
            }
            let key = TaskWaitKey {
                owner: self as *const Self as usize,
                ticket: EVENT_TICKETS
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |ticket| {
                        ticket.checked_add(1)
                    })
                    .expect("task event ticket exhausted"),
                mode,
                deadline_ns,
            };
            self.completion.reset();
            {
                let mut waiter = self.waiter.lock();
                assert!(waiter.is_none(), "TaskEvent allows a single waiter");
                *waiter = Some((target.clone(), key));
            }
            // 登记后复查：signal 若在登记前发布了 pending 且未看到 waiter，由这里收回等待。
            let completed = {
                let mut waiter = self.waiter.lock();
                if self.pending.load(Ordering::Acquire) && waiter.take().is_some() {
                    self.completion.complete();
                    true
                } else {
                    false
                }
            };
            if completed {
                continue;
            }
            let result = target.sleep(&self.completion, key);
            // cancellation 与 signal 的 completion 都在 waiter lock 内完成。
            // 否则旧 signal 摘除 waiter 后延迟 complete，会污染下一 ticket 的 token。
            {
                let mut waiter = self.waiter.lock();
                waiter.take();
                self.completion.complete();
            }
            if matches!(
                result,
                WaitResult::Interrupted | WaitResult::TimedOut | WaitResult::OutOfMemory
            ) {
                return result;
            }
            assert_eq!(result, WaitResult::Woken);
        }
    }

    /// 发布一次 signal；waiter 已阻塞时将其唤醒。重复 signal 合并为一次。
    pub(crate) fn signal(&self) {
        self.signal_with_placement(false);
    }

    /// 发布同步通知，向 scheduler 传递当前 CPU 局部性提示；completion 与 cancellation 协议不变。
    pub(crate) fn signal_on_current_cpu(&self) {
        self.signal_with_placement(true);
    }

    fn signal_with_placement(&self, current_cpu: bool) {
        self.pending.store(true, Ordering::Release);
        let wake = {
            let mut waiter = self.waiter.lock();
            waiter
                .take()
                .and_then(|target| self.completion.complete().then_some(target))
        };
        if let Some((target, key)) = wake {
            if current_cpu {
                target.wake_on_current_cpu(key);
            } else {
                target.wake(key);
            }
        }
    }
}
