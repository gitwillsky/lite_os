#![no_std]
#![feature(allocator_ext)]

//! 系统服务的同步原语。
//!
//! 1. [`Mutex`]：futex 互斥锁，保护短临界区；guard 内不得等待 Linux 任务事件。
//! 2. task mutex / event / rw semaphore：Linux 任务语义的可睡眠原语，等待经 [`TaskWaitTarget`]
//!    落到当前 Linux 线程的 parker，可按策略被信号打断；不承载 Linux 线程的服务线程经
//!    [`ServiceThreadTarget`] 停驻在自身线程描述符上。
//!
//! 服务线程可能在持锁时被内核抢占，因此不使用自旋锁：等待者睡在 futex 上而不是空转。

extern crate alloc;
#[cfg(test)]
extern crate std;

pub use native_runtime::sync::{Mutex, MutexGuard, Once};

mod service_thread;
mod task_event;
mod task_mutex;
mod task_rw_semaphore;
mod task_wait;
mod wait_completion;
pub use service_thread::ServiceThreadTarget;
pub use task_event::TaskEvent;
pub use task_mutex::{TaskMutex, TaskMutexGuard, TaskMutexLockError, TaskMutexWaitPreparation};
pub use task_rw_semaphore::{TaskReadGuard, TaskRwSemaphore, TaskWriteGuard};

pub use task_wait::{TaskWaitKey, TaskWaitTarget, WaitResult, install_wait_target_factory};
pub use wait_completion::WaitCompletion;
