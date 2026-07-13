use super::TaskControlBlock;

const RLIMIT_CPU: usize = 0;
pub(super) const RLIMIT_FSIZE: usize = 1;
pub(crate) const RLIMIT_DATA: usize = 2;
pub(crate) const RLIMIT_STACK: usize = 3;
pub(crate) const RLIMIT_CORE: usize = 4;
pub(crate) const RLIMIT_NPROC: usize = 6;
pub(super) const RLIMIT_NOFILE: usize = 7;
pub(crate) const RLIMIT_MEMLOCK: usize = 8;
pub(crate) const RLIMIT_AS: usize = 9;
const RLIMIT_LOCKS: usize = 10;
const RLIMIT_SIGPENDING: usize = 11;
const RLIMIT_MSGQUEUE: usize = 12;
const RLIMIT_NICE: usize = 13;
const RLIMIT_RTPRIO: usize = 14;
const RLIMIT_RTTIME: usize = 15;
const RESOURCE_COUNT: usize = RLIMIT_RTTIME + 1;

pub(crate) const RLIM_INFINITY: u64 = u64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResourceLimit {
    pub(crate) soft: u64,
    pub(crate) hard: u64,
}

#[derive(Clone)]
pub(super) struct ResourceLimits {
    values: [ResourceLimit; RESOURCE_COUNT],
    // OWNER: Process limits lock 同时拥有最近一次 SIGXCPU 秒数。若放入 Thread，
    // 多线程 Process 会在同一 CPU 秒重复投递 SIGXCPU。
    last_cpu_signal_second: Option<u64>,
}

impl ResourceLimits {
    pub(super) fn defaults() -> Self {
        let unlimited = ResourceLimit {
            soft: RLIM_INFINITY,
            hard: RLIM_INFINITY,
        };
        let mut values = [unlimited; RESOURCE_COUNT];
        values[RLIMIT_STACK] = ResourceLimit {
            soft: 8 * 1024 * 1024,
            hard: RLIM_INFINITY,
        };
        values[RLIMIT_CORE] = ResourceLimit {
            soft: 0,
            hard: RLIM_INFINITY,
        };
        values[RLIMIT_NOFILE] = ResourceLimit {
            soft: 1024,
            hard: crate::fs::MAX_FILE_DESCRIPTORS as u64,
        };
        values[RLIMIT_MEMLOCK] = ResourceLimit {
            soft: 64 * 1024,
            hard: 64 * 1024,
        };
        values[RLIMIT_LOCKS] = unlimited;
        values[RLIMIT_SIGPENDING] = unlimited;
        values[RLIMIT_MSGQUEUE] = unlimited;
        values[RLIMIT_NICE] = ResourceLimit { soft: 0, hard: 0 };
        values[RLIMIT_RTPRIO] = ResourceLimit { soft: 0, hard: 0 };
        values[RLIMIT_RTTIME] = unlimited;
        Self {
            values,
            last_cpu_signal_second: None,
        }
    }

    pub(super) fn get(&self, resource: usize) -> Option<ResourceLimit> {
        self.values.get(resource).copied()
    }

    fn replace(
        &mut self,
        resource: usize,
        replacement: ResourceLimit,
        privileged: bool,
    ) -> Result<ResourceLimit, ResourceLimitError> {
        if replacement.soft > replacement.hard {
            return Err(ResourceLimitError::InvalidLimit);
        }
        if resource == RLIMIT_NOFILE && replacement.hard > crate::fs::MAX_FILE_DESCRIPTORS as u64 {
            return Err(ResourceLimitError::PermissionDenied);
        }
        let current = self
            .values
            .get_mut(resource)
            .ok_or(ResourceLimitError::InvalidResource)?;
        if !privileged && replacement.hard > current.hard {
            return Err(ResourceLimitError::PermissionDenied);
        }
        let old = *current;
        *current = replacement;
        if resource == RLIMIT_CPU {
            self.last_cpu_signal_second = None;
        }
        Ok(old)
    }

    /// @description 根据 Process 累计 CPU 时间生成 Linux RLIMIT_CPU 信号决策。
    ///
    /// @param runtime_us Process 中全部 Thread 累计的 CPU 微秒数。
    /// @return 首次达到 soft limit 及其后每个 CPU 秒返回 SIGXCPU；达到 hard limit 返回 SIGKILL。
    pub(super) fn cpu_signal(&mut self, runtime_us: u64) -> Option<usize> {
        let limit = self.values[RLIMIT_CPU];
        if limit.hard != RLIM_INFINITY && runtime_us >= limit.hard.saturating_mul(1_000_000) {
            return Some(9);
        }
        if limit.soft == RLIM_INFINITY || runtime_us < limit.soft.saturating_mul(1_000_000) {
            return None;
        }
        let elapsed_second = runtime_us / 1_000_000;
        if self
            .last_cpu_signal_second
            .is_some_and(|last| last >= elapsed_second)
        {
            return None;
        }
        self.last_cpu_signal_second = Some(elapsed_second);
        Some(24)
    }

    /// @description fork 复制限制值，但 child 的 CPU 消耗与 SIGXCPU cadence 从零开始。
    pub(super) fn forked(&self) -> Self {
        Self {
            values: self.values,
            last_cpu_signal_second: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceLimitError {
    NotFound,
    PermissionDenied,
    InvalidResource,
    InvalidLimit,
}

impl TaskControlBlock {
    pub(crate) fn file_descriptor_limit(&self) -> usize {
        usize::try_from(
            self.resource_limit(RLIMIT_NOFILE)
                .expect("RLIMIT_NOFILE must exist")
                .soft,
        )
        .unwrap_or(usize::MAX)
        .min(crate::fs::MAX_FILE_DESCRIPTORS)
    }

    pub(crate) fn file_size_limit(&self) -> u64 {
        self.resource_limit(RLIMIT_FSIZE)
            .expect("RLIMIT_FSIZE must exist")
            .soft
    }

    pub(in crate::task) fn process_cpu_runtime_us(&self) -> u64 {
        self.process
            .cpu_runtime_us
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// @description 快照当前 Process 的 stack/address-space fault 边界，供 trap 与 user-copy 共用。
    pub(super) fn user_fault_limits(&self) -> crate::memory::UserFaultLimits {
        crate::memory::UserFaultLimits::new(
            self.resource_limit(RLIMIT_STACK)
                .expect("RLIMIT_STACK must exist")
                .soft,
            self.resource_limit(RLIMIT_AS)
                .expect("RLIMIT_AS must exist")
                .soft,
        )
    }

    pub(crate) fn resource_limit(&self, resource: usize) -> Option<ResourceLimit> {
        self.process.resource_limits.lock().get(resource)
    }

    pub(in crate::task) fn replace_resource_limit(
        &self,
        resource: usize,
        replacement: ResourceLimit,
        privileged: bool,
    ) -> Result<ResourceLimit, ResourceLimitError> {
        self.process
            .resource_limits
            .lock()
            .replace(resource, replacement, privileged)
    }

    pub(in crate::task) fn resource_cpu_signal(&self, runtime_us: u64) -> Option<usize> {
        self.process.resource_limits.lock().cpu_signal(runtime_us)
    }
}
