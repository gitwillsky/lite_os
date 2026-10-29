//! Host scheduler seam: exact TaskWait membership, signal readiness and one wake winner.
use crate::execution::sync::{TaskWaitKey, TaskWaitTarget, WaitCompletion, WaitResult};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use std::{
    cell::RefCell,
    sync::{Condvar, Mutex},
};

thread_local! { static CURRENT: RefCell<Option<Arc<Target>>> = const { RefCell::new(None) }; }
static INSTALL: std::sync::Once = std::sync::Once::new();

pub(crate) fn monotonic_ns() -> u64 {
    static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    CLOCK
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64
}
pub(crate) struct Target {
    pending: AtomicBool,
    fatal: AtomicBool,
    state: Mutex<(Option<TaskWaitKey>, Option<WaitResult>)>,
    changed: Condvar,
}
pub(crate) struct Binding;
impl Drop for Binding {
    fn drop(&mut self) {
        CURRENT.with(|current| current.borrow_mut().take());
    }
}
impl Target {
    pub(crate) fn new() -> Arc<Self> {
        let target = Self {
            pending: false.into(),
            fatal: false.into(),
            state: Mutex::new((None, None)),
            changed: Condvar::new(),
        };
        // 1. macOS 的 std Mutex/Condvar 基于 pthread，首次使用才堆分配。
        // 2. 许多测试在 fail_next 武装后断言被测路径不分配；若 `sleep` 在窗口内第一次加锁，
        //    host 脚手架自己的分配会消耗注入的失败，std 无法容忍而 abort。是否走到 `sleep`
        //    取决于线程竞争，所以表现为概率性崩溃。构造时预热即可与注入窗口隔离。
        drop(target.state.lock().unwrap());
        target.changed.notify_all();
        Arc::new(target)
    }
    pub(crate) fn bind(self: &Arc<Self>) -> Binding {
        INSTALL.call_once(|| {
            crate::execution::sync::install_wait_target_factory(|| {
                CURRENT.with(|current| {
                    current
                        .borrow()
                        .as_ref()
                        .map(|target| target.clone() as Arc<dyn TaskWaitTarget>)
                })
            })
        });
        CURRENT.with(|current| assert!(current.borrow_mut().replace(self.clone()).is_none()));
        Binding
    }
    pub(crate) fn clear_signal(&self) {
        self.pending.store(false, Ordering::Release);
        self.fatal.store(false, Ordering::Release);
    }
    pub(crate) fn fatal_signal(&self) {
        self.fatal.store(true, Ordering::Release);
        self.signal();
    }
    pub(crate) fn signal(&self) {
        self.pending.store(true, Ordering::Release);
        let mut state = self.state.lock().unwrap();
        if state.0.is_some_and(|key| key.signal_pending(self)) {
            state.0 = None;
            state.1 = Some(WaitResult::Interrupted);
            self.changed.notify_all();
        }
    }
    pub(crate) fn capture_wait_key(&self) -> TaskWaitKey {
        self.state.lock().unwrap().0.expect("registered wait")
    }
    pub(crate) fn wait_until_sleeping(&self) {
        let mut state = self.state.lock().unwrap();
        while state.0.is_none() {
            state = self.changed.wait(state).unwrap();
        }
    }
}
impl TaskWaitTarget for Target {
    fn fatal_interrupted(&self) -> bool {
        self.fatal.load(Ordering::Acquire)
    }
    fn interrupted(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) -> WaitResult {
        if !completion.begin_arming() {
            return WaitResult::Woken;
        }
        let mut state = self.state.lock().unwrap();
        assert!(state.0.is_none() && state.1.is_none());
        state.0 = Some(key);
        self.changed.notify_all();
        if completion.finish_arming()
            || key
                .deadline()
                .is_some_and(|deadline| deadline <= monotonic_ns())
            || key.signal_pending(self.as_ref())
        {
            let result = if completion.is_complete() {
                WaitResult::Woken
            } else if key
                .deadline()
                .is_some_and(|deadline| deadline <= monotonic_ns())
            {
                WaitResult::TimedOut
            } else {
                WaitResult::Interrupted
            };
            state.0 = None;
            return result;
        }
        while state.1.is_none() {
            if let Some(deadline) = key.deadline() {
                let remaining = deadline.saturating_sub(monotonic_ns());
                if remaining == 0 {
                    state.0 = None;
                    return WaitResult::TimedOut;
                }
                state = self
                    .changed
                    .wait_timeout(state, std::time::Duration::from_nanos(remaining))
                    .unwrap()
                    .0;
            } else {
                state = self.changed.wait(state).unwrap();
            }
        }
        state.1.take().unwrap()
    }
    fn wake(self: Arc<Self>, key: TaskWaitKey) {
        let mut state = self.state.lock().unwrap();
        if state.0 == Some(key) {
            state.0 = None;
            state.1 = Some(WaitResult::Woken);
            self.changed.notify_all();
        }
    }
}
