//! task mutex 与 task event 共用的 scheduler 阻塞 adapter。

use alloc::sync::Arc;

use super::WaitCompletion;

/// 一次 task-context 阻塞等待的结果；scheduler 与全部可等待对象共用。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WaitResult {
    /// 等待条件已由 owner 发布。
    Woken,
    /// absolute deadline 已到期。
    TimedOut,
    /// 可交付 signal 中断了等待。
    Interrupted,
    /// wait registration 元数据分配失败。
    OutOfMemory,
}

/// task-context 阻塞等待的精确 scheduler membership identity。
///
/// `owner` 是同步对象地址，`ticket` 区分同一对象上的每次等待。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskWaitKey {
    pub(super) owner: usize,
    pub(super) ticket: u64,
}

/// scheduler 为 task mutex 与 task event waiter 提供的 opaque target。
pub(crate) trait TaskWaitTarget: Send + Sync {
    /// 原子发布 membership，并在 completion 尚未发布时阻塞当前 task。
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey);

    /// 消费精确 membership 并使 blocked task 可运行。
    fn wake(self: Arc<Self>, key: TaskWaitKey);
}

type WaitTargetFactory = fn() -> Option<Arc<dyn TaskWaitTarget>>;

// OWNER: task topology 初始化后只安装一次 scheduler adapter；缺失时启动期竞争必须
// fail-stop，不能退回 spin/yield polling。
static WAIT_TARGET_FACTORY: spin::Once<WaitTargetFactory> = spin::Once::new();

/// 安装 task-context 阻塞等待的唯一 scheduler adapter。
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn install_wait_target_factory(factory: WaitTargetFactory) {
    assert!(
        WAIT_TARGET_FACTORY.get().is_none(),
        "task wait target factory installed twice"
    );
    WAIT_TARGET_FACTORY.call_once(|| factory);
}

pub(super) fn current_wait_target() -> Option<Arc<dyn TaskWaitTarget>> {
    if let Some(target) = WAIT_TARGET_FACTORY.get().and_then(|factory| factory()) {
        return Some(target);
    }
    #[cfg(test)]
    {
        Some(Arc::new(TestThreadTarget(std::thread::current())))
    }
    #[cfg(not(test))]
    None
}

#[cfg(test)]
struct TestThreadTarget(std::thread::Thread);

#[cfg(test)]
impl TaskWaitTarget for TestThreadTarget {
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, _key: TaskWaitKey) {
        if !completion.begin_arming() || completion.finish_arming() {
            return;
        }
        while !completion.is_complete() {
            std::thread::park();
        }
    }

    fn wake(self: Arc<Self>, _key: TaskWaitKey) {
        self.0.unpark();
    }
}
