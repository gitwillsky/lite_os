#[path = "../../../kernel/src/sync/wait_completion.rs"]
mod wait_completion;
#[allow(
    unused_imports,
    reason = "共享 scheduler fixture 只导出当前用例需要的实际 owner 接口"
)]
pub(crate) use wait_completion::WaitCompletion;

#[path = "../../../kernel/src/sync/task_wait.rs"]
#[allow(
    dead_code,
    reason = "host tests never construct scheduler wait outcomes"
)]
mod task_wait;
#[allow(
    unused_imports,
    reason = "共享 scheduler fixture 只导出当前用例需要的实际 owner 接口"
)]
pub(crate) use task_wait::{TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory};

#[path = "../../../tools/kernel-view/sync/task_mutex/task_mutex/task_mutex.rs"]
mod task_mutex;
#[allow(
    unused_imports,
    reason = "共享 scheduler fixture 只导出当前用例需要的实际 owner 接口"
)]
pub(crate) use task_mutex::{
    TaskMutex, TaskMutexGuard, TaskMutexLockError, TaskMutexWaitPreparation,
};

#[path = "../../../kernel/src/sync/task_rw_semaphore.rs"]
mod task_rw_semaphore;

#[path = "../../../kernel/src/sync/task_event.rs"]
#[allow(
    dead_code,
    reason = "interruptible events are exercised by separate production-owner integration fixtures"
)]
mod task_event;
#[allow(
    unused_imports,
    reason = "共享 scheduler fixture 只导出当前用例需要的实际 owner 接口"
)]
pub(crate) use task_event::TaskEvent;

use core::sync::atomic::{AtomicU64, Ordering};

// Host fixture 镜像 production 的全局 generation owner；缺少共享序列会让重复 eventfd edge
// 可能比较相等，回归测试便无法覆盖 ET identity。
static READINESS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 为 host kernel-unit fixture 分配单调 readiness generation。
///
/// # Returns
///
/// 本测试进程内不重复的 generation。
pub(crate) fn next_readiness_generation() -> u64 {
    READINESS_GENERATION.fetch_add(1, Ordering::Relaxed)
}
