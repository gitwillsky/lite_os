use alloc::sync::Arc;

use crate::ipc::Pipe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerFdRead {
    Expirations(u64),
    Empty,
}

/// 一个 timer 在 syscall 边界可观察的相对 setting。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TimerSetting {
    pub(crate) remaining_ns: u64,
    pub(crate) interval_ns: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimerError {
    NotFound,
    OutOfMemory,
    Exhausted,
}

/// @description fs 持有 timerfd OFD 时使用的 task-domain backend seam。
///
/// fs 只消费 counter 与 readiness，不反向依赖 task timer queue。最后一个 backend `Arc`
/// 析构时由实现方同步移除 timer record；缺失该 seam 会让 fs → task 形成反向依赖。
pub(crate) trait TimerFdBackend: Send + Sync {
    /// @description 取得 anonymous inode 使用的稳定 runtime identity。
    ///
    /// @return backend 生命周期内唯一 object id。
    fn object_id(&self) -> u64;

    /// @description 原子替换 setting，并清除旧 setting 的未读 expiration。
    ///
    /// @param value_ns 首次到期时间；零表示 disarm。
    /// @param interval_ns 周期；零表示 one-shot。
    /// @param absolute value 是否属于 timer clock 的绝对时间域。
    /// @param now_ns 本次 syscall 的固定 monotonic snapshot。
    /// @return 替换前的相对 setting。
    /// @errors timer 已关闭或 deadline node 分配失败。
    fn replace(
        &self,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
    ) -> Result<TimerSetting, TimerError>;

    /// @description 查询当前相对 setting。
    ///
    /// @param now_ns 本次 syscall 的固定 monotonic snapshot。
    /// @return 当前相对 setting。
    /// @errors timer 已关闭时返回 `NotFound`。
    fn setting(&self, now_ns: u64) -> Result<TimerSetting, TimerError>;

    /// @description 消费全部未读 expiration。
    ///
    /// @return 非零 counter，或当前为空。
    fn read(&self) -> TimerFdRead;

    /// @description 查询 counter 是否非零。
    ///
    /// @return poll read readiness。
    fn readable(&self) -> bool;

    /// @description 取得 poll/epoll 等待的 notification pipe。
    ///
    /// @return 共享 readiness source。
    fn notification_pipe(&self) -> Arc<Pipe>;

    /// @description 查询当前 readiness generation。
    ///
    /// @return notification pipe read generation。
    fn readiness_generation(&self) -> u64;

    /// @description timer queue 在 owner lock 外发布一批到期次数。
    ///
    /// @param elapsed 本次 deadline 跨过的周期数，至少为一。
    fn expire(&self, elapsed: u64);
}

/// @description 在通用 OFD 中保持 thin Arc layout 的 timerfd façade。
///
/// 动态 backend 只藏在本 owner 内；若把 fat trait pointer 直接放进 `OpenFileKind`，会扩大所有
/// OFD 的 hot enum layout，而非只让实际 timerfd 支付间接层与额外 control block 成本。
pub(crate) struct TimerFd {
    backend: Arc<dyn TimerFdBackend>,
}

impl TimerFd {
    /// @description 为 task-domain backend 构造 fs-owned thin façade。
    ///
    /// @param backend timer setting、counter 与 lifecycle 的唯一实现。
    /// @return 可放入通用 OFD 的共享 façade。
    /// @errors façade control block 分配失败。
    pub(crate) fn new(backend: Arc<dyn TimerFdBackend>) -> Result<Arc<Self>, ()> {
        Arc::try_new(Self { backend }).map_err(|_| ())
    }

    pub(crate) fn object_id(&self) -> u64 {
        self.backend.object_id()
    }

    pub(crate) fn replace(
        &self,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
    ) -> Result<TimerSetting, TimerError> {
        self.backend
            .replace(value_ns, interval_ns, absolute, now_ns)
    }

    pub(crate) fn setting(&self, now_ns: u64) -> Result<TimerSetting, TimerError> {
        self.backend.setting(now_ns)
    }

    pub(crate) fn read(&self) -> TimerFdRead {
        self.backend.read()
    }

    pub(crate) fn readable(&self) -> bool {
        self.backend.readable()
    }

    pub(crate) fn notification_pipe(&self) -> Arc<Pipe> {
        self.backend.notification_pipe()
    }

    pub(crate) fn readiness_generation(&self) -> u64 {
        self.backend.readiness_generation()
    }
}
