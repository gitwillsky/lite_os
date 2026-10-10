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

/// 同一等待协议的 signal policy；Killable 只响应 SIGKILL，不撤销普通捕获信号下的 PONR。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TaskWaitMode {
    Uninterruptible,
    /// Linux TASK_IDLE：不可中断的 daemon 空闲等待，TASK_NOLOAD 排除负载统计。
    Idle,
    Interruptible,
    Killable,
}

/// task-context 阻塞等待的精确 scheduler membership identity。
///
/// `owner` 是同步对象地址，`ticket` 区分同一对象上的每次等待。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TaskWaitKey {
    pub(super) owner: usize,
    pub(super) ticket: u64,
    // OWNER: 当前这次等待的 absolute monotonic deadline；无 deadline 不建立 timer/source registration。
    pub(super) deadline_ns: Option<u64>,
    // POLICY: acquisition/maintenance/drain 显式选择 signal policy；DMA/普通 mutex 不取消。
    pub(super) mode: TaskWaitMode,
}

impl TaskWaitKey {
    pub(crate) fn deadline(self) -> Option<u64> {
        self.deadline_ns
    }
    pub(crate) fn event_identity(self) -> (usize, u64) {
        (self.owner, self.ticket)
    }
    /// 实际 native TASK_IDLE wait policy；只读投影，不是另一份 scheduler state。
    pub(crate) fn idle(self) -> bool {
        self.mode == TaskWaitMode::Idle
    }
    pub(crate) fn killable(self) -> bool {
        self.mode == TaskWaitMode::Killable
    }
    pub(crate) fn signal_pending(self, target: &dyn TaskWaitTarget) -> bool {
        match self.mode {
            TaskWaitMode::Uninterruptible | TaskWaitMode::Idle => false,
            TaskWaitMode::Interruptible => target.interrupted(),
            TaskWaitMode::Killable => target.fatal_interrupted(),
        }
    }
    /// Linux TASK_KILLABLE 也包含 TASK_UNINTERRUPTIBLE，因此 fatal-only wait 仍计入 load。
    pub(crate) fn contributes_to_load(self) -> bool {
        matches!(
            self.mode,
            TaskWaitMode::Uninterruptible | TaskWaitMode::Killable
        )
    }
    pub(crate) fn interruptible(self) -> bool {
        matches!(
            self.mode,
            TaskWaitMode::Interruptible | TaskWaitMode::Killable
        )
    }
}

/// scheduler 为 task mutex 与 task event waiter 提供的 opaque target。
pub(crate) trait TaskWaitTarget: Send + Sync {
    /// 原子发布 membership，并在 completion 尚未发布时阻塞当前 task。
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) -> WaitResult;

    /// 查询当前可交付信号；kernel task 与 host adapter 默认没有信号。
    fn interrupted(&self) -> bool {
        false
    }

    /// SIGKILL 已 pending；killable wait 不受普通捕获信号影响。
    fn fatal_interrupted(&self) -> bool {
        false
    }

    /// 消费精确 membership 并使 blocked task 可运行。
    fn wake(self: Arc<Self>, key: TaskWaitKey);

    /// 同步通知的局部性提示；生产 scheduler 在 affinity/active 允许时优先当前 CPU。
    /// 不拥有 task placement，不改变 wait identity；没有逻辑 CPU 的 host adapter 保持普通 wake。
    fn wake_on_current_cpu(self: Arc<Self>, key: TaskWaitKey) {
        self.wake(key);
    }
}

type WaitTargetFactory = fn() -> Option<Arc<dyn TaskWaitTarget>>;

/// 可回滚的 task-context maintenance 在 work unit 之间复查可交付信号。
pub(crate) fn task_interrupted() -> bool {
    current_wait_target().is_some_and(|target| target.interrupted())
}

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
    fn sleep(self: Arc<Self>, completion: &WaitCompletion, key: TaskWaitKey) -> WaitResult {
        if !completion.begin_arming() || completion.finish_arming() {
            return WaitResult::Woken;
        }
        while !completion.is_complete() {
            if let Some(deadline) = key.deadline() {
                // OWNER: host-only wait adapter 的单调时钟原点，不参与生产调度时间。
                static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
                let now = CLOCK
                    .get_or_init(std::time::Instant::now)
                    .elapsed()
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                let remaining = deadline.saturating_sub(now);
                if remaining == 0 {
                    return WaitResult::TimedOut;
                }
                std::thread::park_timeout(std::time::Duration::from_nanos(remaining));
            } else {
                std::thread::park();
            }
        }
        WaitResult::Woken
    }

    fn wake(self: Arc<Self>, _key: TaskWaitKey) {
        self.0.unpark();
    }
}

#[cfg(test)]
mod policy_tests {
    use super::{TaskWaitKey, TaskWaitMode};
    #[test]
    fn native_load_policy_includes_killable_but_excludes_idle_and_interruptible() {
        for (mode, load, interruptible) in [
            (TaskWaitMode::Uninterruptible, true, false),
            (TaskWaitMode::Killable, true, true),
            (TaskWaitMode::Interruptible, false, true),
            (TaskWaitMode::Idle, false, false),
        ] {
            let key = TaskWaitKey {
                owner: 1,
                ticket: 2,
                deadline_ns: None,
                mode,
            };
            assert_eq!(key.contributes_to_load(), load);
            assert_eq!(key.interruptible(), interruptible);
        }
    }
}
