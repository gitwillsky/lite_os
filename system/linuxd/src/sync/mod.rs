//! linuxd 的同步原语。
//!
//! 1. [`Mutex`]：futex 互斥锁，保护短临界区；guard 内不得等待 Linux 任务事件。
//! 2. task mutex / event / rw semaphore：Linux 任务语义的可睡眠原语，等待经 [`TaskWaitTarget`]
//!    落到当前 Linux 线程的 parker，可按策略被信号打断；不承载 Linux 线程的服务线程经
//!    [`ServiceThreadTarget`] 停驻在自身线程描述符上。
//!
//! 服务线程可能在持锁时被内核抢占，因此不使用自旋锁：等待者睡在 futex 上而不是空转。

use core::sync::atomic::{AtomicU64, Ordering};

pub(crate) use native_runtime::sync::{Mutex, MutexGuard};

mod service_thread;
mod task_event;
mod task_mutex;
mod task_rw_semaphore;
mod task_wait;
mod wait_completion;
pub(crate) use service_thread::ServiceThreadTarget;
pub(crate) use task_event::TaskEvent;
pub(crate) use task_mutex::{
    TaskMutex, TaskMutexGuard, TaskMutexLockError, TaskMutexWaitPreparation,
};
pub(crate) use task_rw_semaphore::{TaskReadGuard, TaskRwSemaphore, TaskWriteGuard};

pub(crate) use task_wait::{TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory};
pub(crate) use wait_completion::WaitCompletion;

// OWNER: 该原子只分配跨 I/O source 可比较的 readiness generation，不发布其他内存。
// 缺少全局序列时，嵌套 epoll 无法区分不同 source 上数值相同的局部 generation，ET 会漏报。
static READINESS_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 分配一个跨所有可等待 I/O source 单调递增的 readiness generation。
///
/// # Returns
///
/// 非零 generation；仅用于事件 identity，不承载数据发布同步。
pub(crate) fn next_readiness_generation() -> u64 {
    READINESS_GENERATION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(1)
        })
        .expect("readiness generation exhausted")
}
