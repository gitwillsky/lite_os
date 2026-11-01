//! 时间 namespace 只拥有 monotonic/boottime offset；host clock 与 deadline queue 不复制。
use super::{NamespaceAuthority, NamespaceError, UserNamespace, UserResource, UserResourceCharge};
use alloc::sync::Arc;
use spin::Mutex;
use syscall_abi::capability::Capability;

const SECOND: i128 = 1_000_000_000;
const MAX_SECONDS: i64 = i64::MAX / SECOND as i64;

/// 可受时间 namespace 影响的两个 Linux clock domain；RAW/COARSE 由 ABI 映射到 Monotonic。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeClock {
    Monotonic,
    Boottime,
}
/// 标准归一化 timespec；负值仍以非负 nanoseconds 表示。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeOffset {
    pub(crate) seconds: i64,
    pub(crate) nanoseconds: u32,
}
impl TimeOffset {
    fn total(self) -> i128 {
        i128::from(self.seconds) * SECOND + i128::from(self.nanoseconds)
    }
}
/// 一次操作的 immutable scalar snapshot；绝不保活创建 timer 的 Thread 或保存第二份 namespace。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimeOffsets {
    pub(crate) monotonic: TimeOffset,
    pub(crate) boottime: TimeOffset,
}
impl TimeOffsets {
    pub(crate) const ZERO: Self = Self {
        monotonic: TimeOffset {
            seconds: 0,
            nanoseconds: 0,
        },
        boottime: TimeOffset {
            seconds: 0,
            nanoseconds: 0,
        },
    };
    fn offset(self, clock: TimeClock) -> TimeOffset {
        match clock {
            TimeClock::Monotonic => self.monotonic,
            TimeClock::Boottime => self.boottime,
        }
    }
    /// 将 host timestamp 投影到当前 namespace；offset 在修改时验证当前 clock 范围。
    /// 历史 starttime 可早于负 offset；Linux 的 u64 nanosecond projection 保留二补数结果。
    pub(crate) fn project(self, clock: TimeClock, host: u64) -> u64 {
        (i128::from(host) + self.offset(clock).total()) as u64
    }
    /// absolute deadline 转 host；过期值钳到零，溢出钳到 Linux KTIME_MAX。
    /// 调用者必须先以原始 value 判断 disarm，转换得到零仍是已到期的 armed timer。
    pub(crate) fn to_host(self, clock: TimeClock, timestamp: u64) -> u64 {
        (i128::from(timestamp) - self.offset(clock).total()).clamp(0, i128::from(i64::MAX)) as u64
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeOffsetError {
    Invalid,
    Range,
    Permission,
    Frozen,
}
struct State {
    offsets: TimeOffsets,
    frozen: bool,
}
/// offset 与 freeze bit 的唯一 owner。同锁批量验证/发布；第一次进入后不可再修改。
/// 锁内仅固定大小 scalar 操作，无 IO、分配、调度或 user-copy；收费 token 随实际 owner 回收。
pub(crate) struct TimeNamespace {
    id: u64,
    user: Arc<UserNamespace>,
    state: Mutex<State>,
    _charge: Option<UserResourceCharge>,
}
impl core::fmt::Debug for TimeNamespace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TimeNamespace")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
impl TimeNamespace {
    pub(super) fn initial(user: Arc<UserNamespace>) -> Result<Arc<Self>, NamespaceError> {
        Self::allocate(8, user, TimeOffsets::ZERO, true, None)
    }
    fn allocate(
        id: u64,
        user: Arc<UserNamespace>,
        offsets: TimeOffsets,
        frozen: bool,
        charge: Option<UserResourceCharge>,
    ) -> Result<Arc<Self>, NamespaceError> {
        Arc::try_new(Self {
            id,
            user,
            state: Mutex::new(State { offsets, frozen }),
            _charge: charge,
        })
        .map_err(|_| NamespaceError::OutOfMemory)
    }
    pub(crate) fn copy_for(
        &self,
        user: Arc<UserNamespace>,
        uid: u32,
    ) -> Result<Arc<Self>, NamespaceError> {
        let charge = UserResourceCharge::acquire(&user, uid, UserResource::Time)?;
        Self::allocate(
            super::next_namespace_id(),
            user,
            self.snapshot(),
            false,
            Some(charge),
        )
    }
    pub(crate) fn id(&self) -> u64 {
        self.id
    }
    pub(crate) fn user_namespace(&self) -> Arc<UserNamespace> {
        self.user.clone()
    }
    pub(crate) fn snapshot(&self) -> TimeOffsets {
        self.state.lock().offsets
    }
    /// 在 fork/exec/setns 进入真实 owner 时冻结；重复进入不改变内容。
    pub(crate) fn enter(&self) {
        self.state.lock().frozen = true;
    }
    /// opener 的 f_cred 授权，批量更新两项 offset。host 为本次 syscall 的固定 Mono/Boot 快照。
    /// 非法 clock/timespec、范围、权限或已冻结均无部分写入。
    pub(crate) fn update(
        &self,
        changes: &[(TimeClock, TimeOffset)],
        host: (u64, u64),
        opener: &NamespaceAuthority,
    ) -> Result<(), TimeOffsetError> {
        if !opener.capable(&self.user, Capability::SYS_TIME) {
            return Err(TimeOffsetError::Permission);
        }
        for &(clock, offset) in changes {
            if offset.nanoseconds >= SECOND as u32 {
                return Err(TimeOffsetError::Invalid);
            }
            if !(-MAX_SECONDS..=MAX_SECONDS).contains(&offset.seconds) {
                return Err(TimeOffsetError::Range);
            }
            let now = match clock {
                TimeClock::Monotonic => host.0,
                TimeClock::Boottime => host.1,
            };
            let projected = i128::from(now) + offset.total();
            // 固定 Linux baseline 留下一半 ktime headroom；秒级范围检查允许最后一秒的 nanos。
            if projected < 0 || projected / SECOND > i128::from(MAX_SECONDS / 2) {
                return Err(TimeOffsetError::Range);
            }
        }
        let mut state = self.state.lock();
        if state.frozen {
            return Err(TimeOffsetError::Frozen);
        }
        for &(clock, offset) in changes {
            match clock {
                TimeClock::Monotonic => state.offsets.monotonic = offset,
                TimeClock::Boottime => state.offsets.boottime = offset,
            }
        }
        Ok(())
    }
}
