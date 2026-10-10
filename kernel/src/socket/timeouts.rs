//! 唯一 socket 收发超时 policy；阻塞操作在上层冻结一次 host-monotonic deadline。
use spin::Mutex;

/// 需要冻结超时的收发方向。
#[derive(Clone, Copy)]
pub(crate) enum SocketIoDirection {
    Receive,
    Send,
}

/// 零 timeval 禁用超时；负秒数表示立即超时，两者的 getsockopt 投影均为零。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketTimeout {
    Infinite,
    Finite(u64),
}
impl SocketTimeout {
    /// 将有限时长向 scheduler tick 上取整；极大合法输入在 KTIME_MAX 饱和。
    /// `duration_ns == 0` 是立即超时，禁用必须显式传 Infinite。
    pub(crate) fn finite(duration_ns: u128) -> Self {
        let tick = u128::from(crate::timer::SCHEDULER_TICK_NS);
        Self::Finite(
            duration_ns
                .div_ceil(tick)
                .saturating_mul(tick)
                .min(i64::MAX as u128) as u64,
        )
    }
    /// 在操作开始时从唯一 host-monotonic clock 生成 deadline；重试复用返回值。
    pub(crate) fn deadline(self, now_ns: u64) -> Option<u64> {
        match self {
            Self::Infinite => None,
            Self::Finite(duration) => Some(now_ns.saturating_add(duration).min(i64::MAX as u64)),
        }
    }
    /// 返回 native timeval getter 的微秒投影。
    pub(crate) fn microseconds(self) -> u64 {
        match self {
            Self::Infinite => 0,
            Self::Finite(duration) => duration / 1_000,
        }
    }
}
#[derive(Clone, Copy)]
pub(super) struct TimeoutValues {
    receive: SocketTimeout,
    send: SocketTimeout,
}

/// OFD facade 与 INET registry 的 Arc 引用同一超时 owner；dup/fork 不复制 live policy。
/// INET setter 按 operation → stack → policy 加锁，passive child 在握手完成时冻结值。
/// 此锁不持有 backend/registry guard，不缓存 deadline 或等待状态。
pub(super) struct SocketTimeouts(Mutex<TimeoutValues>);
impl SocketTimeouts {
    /// 构造原生无限等待初态，不分配。
    pub(super) const fn new() -> Self {
        Self(Mutex::new(TimeoutValues {
            receive: SocketTimeout::Infinite,
            send: SocketTimeout::Infinite,
        }))
    }
    /// 冻结实际 pending child 的两个值；此不可变快照随协议 handle 转移。
    pub(super) fn snapshot(&self) -> TimeoutValues {
        *self.0.lock()
    }
    /// 初始化尚未发布的 accepted policy；只能由消耗 pending child 的 owner 调用。
    pub(super) fn initialize(&self, values: TimeoutValues) {
        *self.0.lock() = values;
    }
    /// 更新唯一方向值；既有阻塞操作继续使用已经冻结的 deadline。
    pub(super) fn set(&self, direction: SocketIoDirection, value: SocketTimeout) {
        let mut values = self.0.lock();
        match direction {
            SocketIoDirection::Receive => values.receive = value,
            SocketIoDirection::Send => values.send = value,
        }
    }
    /// 投影一次操作所需的实际 policy。
    pub(super) fn get(&self, direction: SocketIoDirection) -> SocketTimeout {
        let values = self.0.lock();
        match direction {
            SocketIoDirection::Receive => values.receive,
            SocketIoDirection::Send => values.send,
        }
    }
}
