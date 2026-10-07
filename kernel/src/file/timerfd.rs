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

/// fs 持有 timerfd OFD 时使用的 task-domain backend seam。
///
/// fs 只消费 counter 与 readiness，不反向依赖 task timer queue。最后一个 backend `Arc`
/// 析构时由实现方同步移除 timer record；缺失该 seam 会让 fs → task 形成反向依赖。
pub(crate) trait TimerFdBackend: Send + Sync {
    /// 取得 anonymous inode 使用的稳定 runtime identity。
    ///
    /// # Returns
    ///
    /// backend 生命周期内唯一 object id。
    fn object_id(&self) -> u64;

    /// 原子替换 setting，并清除旧 setting 的未读 expiration。
    ///
    /// # Parameters
    ///
    /// - `value_ns`: 首次到期时间；零表示 disarm。
    /// - `interval_ns`: 周期；零表示 one-shot。
    /// - `absolute`: value 是否属于 timer clock 的绝对时间域。
    /// - `now_ns`: 本次 syscall 的固定 monotonic snapshot。
    ///
    /// # Returns
    ///
    /// 替换前的相对 setting。
    ///
    /// # Errors
    ///
    /// timer 已关闭或 deadline node 分配失败。
    fn replace(
        &self,
        value_ns: u64,
        interval_ns: u64,
        absolute: bool,
        now_ns: u64,
    ) -> Result<TimerSetting, TimerError>;

    /// 查询当前相对 setting。
    ///
    /// # Parameters
    ///
    /// - `now_ns`: 本次 syscall 的固定 monotonic snapshot。
    ///
    /// # Returns
    ///
    /// 当前相对 setting。
    ///
    /// # Errors
    ///
    /// timer 已关闭时返回 `NotFound`。
    fn setting(&self, now_ns: u64) -> Result<TimerSetting, TimerError>;

    /// 消费全部未读 expiration。
    ///
    /// # Returns
    ///
    /// 非零 counter，或当前为空。
    fn read(&self) -> TimerFdRead;

    /// 查询 counter 是否非零。
    ///
    /// # Returns
    ///
    /// poll read readiness。
    fn readable(&self) -> bool;

    /// 取得 poll/epoll 等待的 notification pipe。
    ///
    /// # Returns
    ///
    /// 共享 readiness source。
    fn notification_pipe(&self) -> Arc<Pipe>;

    /// 查询当前 readiness generation。
    ///
    /// # Returns
    ///
    /// notification pipe read generation。
    fn readiness_generation(&self) -> u64;

    /// timer queue 在 owner lock 外发布一批到期次数。
    ///
    /// # Parameters
    ///
    /// - `elapsed`: 本次 deadline 跨过的周期数，至少为一。
    fn expire(&self, elapsed: u64);
}

/// 在通用 OFD 中保持 thin Arc layout 的 timerfd façade。
///
/// 动态 backend 只藏在本 owner 内；若把 fat trait pointer 直接放进 `OpenFileKind`，会扩大所有
/// OFD 的 hot enum layout，而非只让实际 timerfd 支付间接层与额外 control block 成本。
pub(crate) struct TimerFd {
    backend: Arc<dyn TimerFdBackend>,
}

impl TimerFd {
    /// 为 task-domain backend 构造 fs-owned thin façade。
    ///
    /// # Parameters
    ///
    /// - `backend`: timer setting、counter 与 lifecycle 的唯一实现。
    ///
    /// # Returns
    ///
    /// 可放入通用 OFD 的共享 façade。
    ///
    /// # Errors
    ///
    /// façade control block 分配失败。
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
