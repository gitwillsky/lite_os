#[path = "../../../system/service-sync/src/wait_completion.rs"]
mod wait_completion;
#[allow(
    unused_imports,
    reason = "共享 scheduler fixture 只导出当前用例需要的实际 owner 接口"
)]
pub(crate) use wait_completion::WaitCompletion;

#[path = "../../../system/service-sync/src/task_wait.rs"]
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

#[path = "../../../tools/source-view/service-sync/task_mutex/task_mutex/task_mutex.rs"]
#[expect(
    dead_code,
    reason = "partial scheduler assembly; get_mut and signal-aware locking run in dedicated integration fixtures"
)]
mod task_mutex;
#[allow(
    unused_imports,
    reason = "共享 scheduler fixture 只导出当前用例需要的实际 owner 接口"
)]
pub(crate) use task_mutex::{
    TaskMutex, TaskMutexGuard, TaskMutexLockError, TaskMutexWaitPreparation,
};

#[path = "../../../system/service-sync/src/task_rw_semaphore.rs"]
mod task_rw_semaphore;

#[path = "../../../system/service-sync/src/task_event.rs"]
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

#[allow(
    unused_imports,
    reason = "宿主调度适配替代 native futex 等待；互斥算法仍执行生产实现"
)]
pub(crate) use spin::{Mutex, MutexGuard, Once};

/// Host scheduler seam for the driver's real completion owner; no native syscalls on the host.
pub(crate) struct ServiceThreadTarget(std::thread::Thread);
impl ServiceThreadTarget {
    pub(crate) fn current() -> alloc::sync::Arc<Self> {
        alloc::sync::Arc::new(Self(std::thread::current()))
    }
    pub(crate) fn park_until(&self, completion: &WaitCompletion, _: Option<u64>) {
        while !completion.is_complete() {
            std::thread::park();
        }
    }
    pub(crate) fn unpark(&self) {
        self.0.unpark();
    }
}

impl TaskWaitTarget for ServiceThreadTarget {
    fn sleep(
        self: alloc::sync::Arc<Self>,
        completion: &WaitCompletion,
        key: TaskWaitKey,
    ) -> WaitResult {
        assert!(
            key.deadline().is_none(),
            "untimed host mutex seam does not emulate a guest clock"
        );
        self.park_until(completion, None);
        WaitResult::Woken
    }
    fn wake(self: alloc::sync::Arc<Self>, _: TaskWaitKey) {
        self.unpark();
    }
}
