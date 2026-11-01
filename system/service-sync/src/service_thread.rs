//! 服务线程（启动线程、pager worker、deferred worker 等不承载 Linux 线程的 native 线程）的阻塞等待目标。
//!
//! 1. 等待者先以 [`WaitCompletion`] 的 arming 握手发布睡眠意图，再停驻在自身线程描述符上；
//! 2. owner 完成后 `wake` 交付一次 unpark；停驻令牌合并多次唤醒，所以完成先于停驻也不会丢失；
//! 3. 服务线程没有 Linux 信号，等待只会被完成或 deadline 结束。

use alloc::sync::Arc;

use super::{TaskWaitKey, TaskWaitTarget, WaitCompletion, WaitResult};

/// 当前服务线程的停驻器。
pub struct ServiceThreadTarget {
    #[cfg(not(test))]
    thread: Arc<native_runtime::thread::Descriptor>,
    #[cfg(test)]
    thread: std::thread::Thread,
}

impl ServiceThreadTarget {
    /// 调用线程的等待目标。
    pub fn current() -> Arc<Self> {
        Arc::new(Self {
            #[cfg(not(test))]
            thread: native_runtime::thread::current_arc(),
            #[cfg(test)]
            thread: std::thread::current(),
        })
    }

    /// 停驻直到 `completion` 完成或到达 `deadline_ns`（单调时钟纳秒）。
    pub fn park_until(&self, completion: &WaitCompletion, deadline_ns: Option<u64>) -> WaitResult {
        if !completion.begin_arming() || completion.finish_arming() {
            return WaitResult::Woken;
        }
        while !completion.is_complete() {
            if let Some(deadline) = deadline_ns
                && deadline <= (native_runtime::system::monotonic() as u64)
            {
                return WaitResult::TimedOut;
            }
            self.park(deadline_ns);
        }
        WaitResult::Woken
    }

    #[cfg(not(test))]
    fn park(&self, deadline_ns: Option<u64>) {
        let deadline = deadline_ns.map_or(native_runtime::native_abi::DEADLINE_INFINITE, |deadline| {
            i64::try_from(deadline).unwrap_or(i64::MAX)
        });
        let _ = self.thread.park(deadline);
    }

    #[cfg(test)]
    fn park(&self, deadline_ns: Option<u64>) {
        match deadline_ns {
            Some(deadline) => std::thread::park_timeout(std::time::Duration::from_nanos(
                deadline.saturating_sub((native_runtime::system::monotonic() as u64)),
            )),
            None => std::thread::park(),
        }
    }

    /// 交付一次唤醒。
    pub fn unpark(&self) {
        self.thread.unpark();
    }
}

impl TaskWaitTarget for ServiceThreadTarget {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) -> WaitResult {
        self.park_until(completion, key.deadline())
    }

    fn wake(self: Arc<Self>, _key: TaskWaitKey) {
        self.unpark();
    }
}
