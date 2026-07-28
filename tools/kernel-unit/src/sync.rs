#[path = "../../../kernel/src/sync/wait_completion.rs"]
mod wait_completion;
pub(crate) use wait_completion::WaitCompletion;

#[path = "../../../kernel/src/sync/task_mutex.rs"]
mod task_mutex;
pub(crate) use task_mutex::{TaskMutex, TaskMutexGuard};

use core::sync::atomic::{AtomicU64, Ordering};

// Host fixture 镜像 production 的全局 generation owner；缺少共享序列会让重复 eventfd edge
// 可能比较相等，回归测试便无法覆盖 ET identity。
static READINESS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// @description 为 host kernel-unit fixture 分配单调 readiness generation。
/// @return 本测试进程内不重复的 generation。
pub(crate) fn next_readiness_generation() -> u64 {
    READINESS_GENERATION.fetch_add(1, Ordering::Relaxed)
}
