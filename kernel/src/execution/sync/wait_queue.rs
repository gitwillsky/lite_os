//! 多等待者、条件驱动的等待队列。
//!
//! 等待者先登记再复查条件，唤醒者先改变条件再唤醒全部登记者；两侧顺序相反，
//! 因此任何交错下都不会丢失唤醒。唤醒是广播：每个等待者醒来后自行复查条件。

use alloc::sync::Arc;
use alloc::vec::Vec;

use spin::Mutex;

use super::{TaskEvent, WaitResult, lock_with_spare};

/// 条件变量式的广播等待队列。
pub(crate) struct WaitQueue {
    // OWNER: 只保存当前阻塞中的等待者；唤醒时整体取走，缺失登记会让条件变化后等待者永久睡眠。
    waiters: Mutex<Vec<Arc<TaskEvent>>>,
}

impl WaitQueue {
    /// 构造空队列。
    pub(crate) const fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::new()),
        }
    }

    /// 阻塞直到 `ready` 返回 `true`、截止时间到达或当前线程被打断。
    ///
    /// # Parameters
    ///
    /// - `ready`: 在登记后与每次唤醒后复查的条件；不得阻塞。
    /// - `deadline_ns`: 单调时钟绝对截止时间。
    ///
    /// # Returns
    ///
    /// 条件满足返回 `Woken`；否则返回 `TimedOut`、`Interrupted` 或 `OutOfMemory`。
    pub(crate) fn wait_until(
        &self,
        mut ready: impl FnMut() -> bool,
        deadline_ns: Option<u64>,
    ) -> WaitResult {
        loop {
            if ready() {
                return WaitResult::Woken;
            }
            let Ok(event) = Arc::try_new(TaskEvent::new()) else {
                return WaitResult::OutOfMemory;
            };
            {
                let Ok(mut waiters) = lock_with_spare(&self.waiters, |_| 1, |waiters| waiters)
                else {
                    return WaitResult::OutOfMemory;
                };
                waiters.push(event.clone());
            }
            // 登记后复查：条件若在登记前已变化，唤醒者可能没有看到本等待者。
            if ready() {
                self.remove(&event);
                return WaitResult::Woken;
            }
            let result = match deadline_ns {
                Some(deadline) => event.wait_interruptible_until(deadline),
                None => event.wait_interruptible(),
            };
            self.remove(&event);
            match result {
                WaitResult::Woken => {}
                other => {
                    return if ready() { WaitResult::Woken } else { other };
                }
            }
        }
    }

    /// 唤醒全部登记的等待者。调用者必须先发布条件变化。
    pub(crate) fn wake_all(&self) {
        let waiters = core::mem::take(&mut *self.waiters.lock());
        for waiter in &waiters {
            waiter.signal();
        }
    }

    fn remove(&self, event: &Arc<TaskEvent>) {
        self.waiters
            .lock()
            .retain(|waiter| !Arc::ptr_eq(waiter, event));
    }
}
