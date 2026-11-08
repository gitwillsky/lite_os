//! EDF 的 CBS 服务预算；只保存时间/额度，不拥有线程、队列或 Linux 策略。
//! 隐式截止采用 CBS 唤醒规则，约束截止采用 Revisited CBS；耗尽后在下一周期开始补充，超额作为债务保留。

/// 一个受准入控制的截止时间配置，单位纳秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Parameters {
    runtime: u64,
    deadline: u64,
    period: u64,
}

impl Parameters {
    /// 检查约束 0 < runtime <= deadline <= period；有符号时钟范围外不可表达。
    pub(crate) fn new(runtime: u64, deadline: u64, period: u64) -> Option<Self> {
        (runtime != 0 && runtime <= deadline && deadline <= period && period <= i64::MAX as u64)
            .then_some(Self {
                runtime,
                deadline,
                period,
            })
    }

    /// 向上取整的 Q32 密度。按 runtime/deadline 准入，比只检查利用率更保守，支持约束截止。
    pub(crate) fn density(self) -> u64 {
        ((u128::from(self.runtime) << 32).div_ceil(u128::from(self.deadline))) as u64
    }
}

/// 一个配置对应的唯一预算 owner；remaining 可为负，不能因延迟抢占抹掉执行债务。
pub(crate) struct Cbs {
    parameters: Parameters,
    deadline: u64,
    remaining: i64,
}

impl Cbs {
    pub(crate) fn parameters(&self) -> Parameters {
        self.parameters
    }

    pub(crate) fn new(parameters: Parameters, now: u64) -> Self {
        Self {
            parameters,
            deadline: now.saturating_add(parameters.deadline),
            remaining: parameters.runtime as i64,
        }
    }

    /// 真正阻塞后的激活才可重置服务期限；预算耗尽不走唤醒信用捷径。
    pub(crate) fn wake(&mut self, now: u64) {
        if self.remaining <= 0 {
            self.replenish(now);
            if self.remaining <= 0 {
                return;
            }
        }
        if self.deadline <= now {
            if now < self.replenishment() {
                // 约束截止已错过、下一周期尚未开始：不能通过重新唤醒取得提前预算。
                self.remaining = 0;
                return;
            }
            self.deadline = now.saturating_add(self.parameters.deadline);
            self.remaining = self.parameters.runtime as i64;
            return;
        }
        let distance = self.deadline - now;
        if (self.remaining as u128) * u128::from(self.parameters.deadline)
            > u128::from(distance) * u128::from(self.parameters.runtime)
        {
            if self.parameters.deadline == self.parameters.period {
                self.deadline = now.saturating_add(self.parameters.deadline);
                self.remaining = self.parameters.runtime as i64;
            } else {
                // Abeni/Lipari/Lelli 的 revised wakeup：只减少剩余额度，不重新授予完整周期。
                self.remaining = (u128::from(distance) * u128::from(self.parameters.runtime)
                    / u128::from(self.parameters.deadline)) as i64;
            }
        }
    }

    /// 已消耗执行时间，包括 restricted 与服务端路径；不会向预算回退或重复计费。
    pub(crate) fn charge(&mut self, runtime: u64) {
        self.remaining =
            (i128::from(self.remaining) - i128::from(runtime)).max(i128::from(i64::MIN)) as i64;
    }

    /// 主动让出本周期，保留已经发生的超额债务；抢占不能调用它。
    pub(crate) fn forfeit(&mut self) {
        self.remaining = self.remaining.min(0);
    }

    /// 补充已经到达的周期。大跨度用整数计算一次推进，不逐周期循环；从不提前赊取未来额度。
    pub(crate) fn replenish(&mut self, now: u64) {
        let replenishment = self.replenishment();
        if self.remaining > 0 || now < replenishment {
            return;
        }
        let periods = 1 + (now - replenishment) / self.parameters.period;
        self.deadline = self
            .deadline
            .saturating_add(periods.saturating_mul(self.parameters.period));
        self.remaining = (i128::from(self.remaining)
            + i128::from(periods) * i128::from(self.parameters.runtime))
        .min(i128::from(self.parameters.runtime)) as i64;
        if self.deadline <= now {
            // 大跨度定时延迟落在已过的约束截止之后，不把错过期限的剩余额度提前挪到下周期。
            self.remaining = self.remaining.min(0);
        }
    }

    pub(crate) fn deadline(&self) -> u64 {
        self.deadline
    }

    /// 无额度时的下一补充时刻；不是通用阻塞，不消费应用自己的等待身份。
    pub(crate) fn not_before(&self) -> Option<u64> {
        (self.remaining <= 0).then(|| self.replenishment())
    }

    fn replenishment(&self) -> u64 {
        self.deadline
            .saturating_add(self.parameters.period - self.parameters.deadline)
    }

    /// 当前 active 区间起点加尚未提交的预算；即使没有竞争者也必须武装这个截止。
    pub(crate) fn budget_limit(&self, active_start: u64) -> u64 {
        active_start.saturating_add(self.remaining.max(0) as u64)
    }
}

/// 截止类先于公平类；相同类的排序值越小越早，不把补充时刻当成 EDF 截止。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadyKey {
    Fair(u64),
    Deadline {
        deadline: u64,
        not_before: Option<u64>,
    },
}
impl ReadyKey {
    pub(crate) fn eligible(self, sample: impl FnOnce() -> u64) -> bool {
        match self {
            Self::Fair(_) => true,
            Self::Deadline { not_before, .. } => not_before.is_none_or(|time| time <= sample()),
        }
    }
    fn rank(self) -> (u8, u64) {
        match self {
            Self::Deadline { deadline, .. } => (0, deadline),
            Self::Fair(vruntime) => (1, vruntime),
        }
    }
}
impl Ord for ReadyKey {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.rank()
            .cmp(&other.rank())
            .then_with(|| match (*self, *other) {
                (Self::Deadline { not_before: a, .. }, Self::Deadline { not_before: b, .. }) => {
                    a.cmp(&b)
                }
                _ => core::cmp::Ordering::Equal,
            })
    }
}
impl PartialOrd for ReadyKey {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
