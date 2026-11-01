//! task mutex 与 task event 共用的 scheduler 阻塞 adapter。

use alloc::sync::Arc;

use super::WaitCompletion;

/// 一次 task-context 阻塞等待的结果；scheduler 与全部可等待对象共用。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum WaitResult {
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
pub struct TaskWaitKey {
    pub(super) owner: usize,
    pub(super) ticket: u64,
    // OWNER: 当前这次等待的 absolute monotonic deadline；无 deadline 不建立 timer/source registration。
    pub(super) deadline_ns: Option<u64>,
    // POLICY: acquisition/maintenance/drain 显式选择 signal policy；DMA/普通 mutex 不取消。
    pub(super) mode: TaskWaitMode,
}

impl TaskWaitKey {
    pub fn deadline(self) -> Option<u64> {
        self.deadline_ns
    }
    pub fn event_identity(self) -> (usize, u64) {
        (self.owner, self.ticket)
    }
    /// 实际 native TASK_IDLE wait policy；只读投影，不是另一份 scheduler state。
    pub fn idle(self) -> bool {
        self.mode == TaskWaitMode::Idle
    }
    pub fn killable(self) -> bool {
        self.mode == TaskWaitMode::Killable
    }
    pub fn signal_pending(self, target: &dyn TaskWaitTarget) -> bool {
        match self.mode {
            TaskWaitMode::Uninterruptible | TaskWaitMode::Idle => false,
            TaskWaitMode::Interruptible => target.interrupted(),
            TaskWaitMode::Killable => target.fatal_interrupted(),
        }
    }
    /// Linux TASK_KILLABLE 也包含 TASK_UNINTERRUPTIBLE，因此 fatal-only wait 仍计入 load。
    pub fn contributes_to_load(self) -> bool {
        matches!(
            self.mode,
            TaskWaitMode::Uninterruptible | TaskWaitMode::Killable
        )
    }
    pub fn interruptible(self) -> bool {
        matches!(
            self.mode,
            TaskWaitMode::Interruptible | TaskWaitMode::Killable
        )
    }
}

/// scheduler 为 task mutex 与 task event waiter 提供的 opaque target。
pub trait TaskWaitTarget: Send + Sync {
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

// OWNER: task topology 初始化后只安装一次的 Linux 线程 adapter；缺失时 Linux 线程的等待会落到
// 服务线程停驻器上，信号无法打断等待，也不进入 Linux 调度状态。
static WAIT_TARGET_FACTORY: super::Once<WaitTargetFactory> = super::Once::new();

/// 安装 task-context 阻塞等待的唯一 scheduler adapter。
#[cfg_attr(test, allow(dead_code))]
pub fn install_wait_target_factory(factory: WaitTargetFactory) {
    assert!(
        WAIT_TARGET_FACTORY.get().is_none(),
        "task wait target factory installed twice"
    );
    WAIT_TARGET_FACTORY.call_once(|| factory);
}

/// 调用线程的等待目标：承载 Linux 线程时为该线程，否则为服务线程停驻器。
pub(super) fn current_wait_target() -> Arc<dyn TaskWaitTarget> {
    WAIT_TARGET_FACTORY
        .get()
        .and_then(|factory| factory())
        .unwrap_or_else(|| super::ServiceThreadTarget::current())
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
