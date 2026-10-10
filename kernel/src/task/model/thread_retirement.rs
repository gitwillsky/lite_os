//! 从 Thread publication 到 MM/vfork completion 的唯一在途生命周期计数。
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::sync::{TaskEvent, WaitResult};

/// Process 独占的在途 Thread owner；不同于 graph live membership，包含已摘图的清理线程。
/// 初始 Thread 计一次，所有后续 publication 在 graph guard 内登记；计数从不在 exec 时重置。
pub(in crate::task) struct ThreadRetirements {
    outstanding: AtomicUsize,
    completed: TaskEvent,
}
impl ThreadRetirements {
    pub(super) fn new() -> Self {
        Self {
            outstanding: AtomicUsize::new(1),
            completed: TaskEvent::new(),
        }
    }
    /// 在 graph publication 前登记一次；失败的 unpublished child 不得进入计数。
    pub(in crate::task) fn publish_thread(&self) {
        let previous = self.outstanding.fetch_add(1, Ordering::Relaxed);
        assert!(
            previous != 0 && previous != usize::MAX,
            "invalid thread publication lifetime"
        );
    }
    /// 每个 graph removal 唯一领取，是否已有 exec claim 不影响清理 obligation。
    pub(in crate::task) fn retirement(self: &Arc<Self>) -> ThreadRetirement {
        ThreadRetirement(self.clone())
    }
    /// 只允许 calling survivor 留在计数中；SIGKILL 可取消，不返回普通 signal interruption。
    pub(in crate::task) fn wait_for_survivor(&self) -> WaitResult {
        loop {
            let outstanding = self.outstanding.load(Ordering::Acquire);
            assert!(outstanding != 0, "exec caller lifetime disappeared");
            if outstanding == 1 {
                return WaitResult::Woken;
            }
            if self.completed.wait_killable() == WaitResult::Interrupted {
                return WaitResult::Interrupted;
            }
        }
    }
}
/// graph 摘除线程的单次完成 token；由 exit 路径在 MM 与原 vfork wake 全部完成后消费。
#[must_use = "Thread removal must complete its MM/vfork retirement"]
pub(in crate::task) struct ThreadRetirement(Arc<ThreadRetirements>);
impl ThreadRetirement {
    /// 返回是否完成该 Process 的最后一个实际执行体；只有该 caller 可发表 process exit。
    pub(in crate::task) fn complete(self) -> bool {
        let previous = self.0.outstanding.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "Thread lifetime completed twice");
        if previous == 2 {
            self.0.completed.signal();
        }
        previous == 1
    }
}
impl super::TaskControlBlock {
    pub(in crate::task) fn thread_retirements(&self) -> &Arc<ThreadRetirements> {
        &self.process().retirements
    }
}
