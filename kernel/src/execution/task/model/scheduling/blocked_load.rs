//! scheduler 实际不可中断 Blocked membership 的计数 owner；一个 token 对应一个已切出的执行体。
use core::sync::atomic::{AtomicUsize, Ordering};

// OWNER: 所有实际 BlockedLoad token 的唯一 aggregate；token 只归 SchedulingState，不能复制。
// register/Drop 在同一个 scheduling guard 下随状态转移，无手动增减入口或 run-state shadow。
// Relaxed sample 与 Linux distributed load accounting 一样允许瞬时跨 CPU 转移误差。
static BLOCKED: AtomicUsize = AtomicUsize::new(0);

/// 不可复制的不可中断 blocked admission；wake/stop/exit 退休时自动归还。
#[derive(Debug)]
pub(super) struct BlockedLoad;
impl BlockedLoad {
    pub(super) fn register() -> Self {
        let previous = BLOCKED.fetch_add(1, Ordering::Relaxed);
        assert_ne!(
            previous,
            usize::MAX,
            "blocked task admission count overflow"
        );
        Self
    }
}
impl Drop for BlockedLoad {
    fn drop(&mut self) {
        let previous = BLOCKED.fetch_sub(1, Ordering::Relaxed);
        assert_ne!(previous, 0, "blocked task admission count underflow");
    }
}
/// 读取实际 blocked admissions；不分配、不关闭 IRQ、不扫描任何 TCB 或 wait registry。
pub(in crate::execution::task) fn uninterruptible_task_count() -> usize {
    BLOCKED.load(Ordering::Relaxed)
}
