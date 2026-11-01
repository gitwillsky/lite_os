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
pub(in crate::task) const RLIMIT_SIGPENDING: usize = 11;
const RLIMIT_MSGQUEUE: usize = 12;
pub(in crate::task) const RLIMIT_NICE: usize = 13;
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
            hard: crate::file::MAX_FILE_DESCRIPTORS as u64,
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

    pub(super) fn cpu_limit_active(&self) -> bool {
        let limit = self.values[RLIMIT_CPU];
        limit.soft != RLIM_INFINITY || limit.hard != RLIM_INFINITY
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
        if resource == RLIMIT_NOFILE && replacement.hard > crate::file::MAX_FILE_DESCRIPTORS as u64
        {
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

    /// 根据 Process 累计 CPU 时间生成 Linux RLIMIT_CPU 信号决策。
    ///
    /// # Parameters
    ///
    /// - `runtime_us`: Process 中全部 Thread 累计的 CPU 微秒数。
    ///
    /// # Returns
    ///
    /// 首次达到 soft limit 及其后每个 CPU 秒返回 SIGXCPU；达到 hard limit 返回 SIGKILL。
    pub(super) fn cpu_signal(&mut self, runtime_us: u64) -> Option<usize> {
        let limit = self.values[RLIMIT_CPU];
        if limit.hard != RLIM_INFINITY && runtime_us >= limit.hard.saturating_mul(1_000_000) {
            return Some(syscall_abi::signal::SIGKILL);
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
        Some(syscall_abi::signal::SIGXCPU)
    }

    /// fork 复制限制值，但 child 的 CPU 消耗与 SIGXCPU cadence 从零开始。
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

mod unix_file_limit;

impl TaskControlBlock {
    /// SCM 只借实际 Process limits allocation；fork 已复制、Thread 共享、exec 保留。
    /// Arc clone/type erasure 不分配，排队 control 不保活 Process/MM/PID quota。
    pub(crate) fn unix_file_limit(&self) -> alloc::sync::Arc<dyn crate::net::UnixFileLimit> {
        self.process().resource_limits.clone()
    }

    pub(crate) fn file_descriptor_limit(&self) -> usize {
        usize::try_from(
            self.resource_limit(RLIMIT_NOFILE)
                .expect("RLIMIT_NOFILE must exist")
                .soft,
        )
        .unwrap_or(usize::MAX)
        .min(crate::file::MAX_FILE_DESCRIPTORS)
    }

    pub(crate) fn file_size_limit(&self) -> u64 {
        self.resource_limit(RLIMIT_FSIZE)
            .expect("RLIMIT_FSIZE must exist")
            .soft
    }

    pub(in crate::task) fn process_cpu_runtime_us(&self) -> u64 {
        self.process_cpu_time().0
    }

    /// NEWUSER 的全部已使用 rlimit maxima 由同一实际 Process limits 快照捕获。
    pub(super) fn namespace_rlimits(&self) -> crate::namespace::NamespaceRlimits {
        let limits = self.process().resource_limits.lock();
        crate::namespace::NamespaceRlimits {
            locked_memory: limits
                .get(RLIMIT_MEMLOCK)
                .expect("RLIMIT_MEMLOCK exists")
                .soft,
            pending_signals: limits
                .get(RLIMIT_SIGPENDING)
                .expect("RLIMIT_SIGPENDING exists")
                .soft,
        }
    }

    /// 同一 actual limits guard 捕获完整 MM admission tuple，不能将并发 prlimit 的两次读取拼接。
    pub(super) fn mapping_resource_limits(&self) -> crate::memory::MappingResourceLimits {
        let limits = self.process().resource_limits.lock();
        crate::memory::MappingResourceLimits::new(
            limits.get(RLIMIT_AS).expect("RLIMIT_AS exists").soft,
            limits.get(RLIMIT_DATA).expect("RLIMIT_DATA exists").soft,
        )
    }

    pub(crate) fn resource_limit(&self, resource: usize) -> Option<ResourceLimit> {
        self.process().resource_limit(resource)
    }

    pub(in crate::task) fn resource_cpu_signal(&self, runtime_us: u64) -> Option<usize> {
        if !self.process().cpu_limit_active.get() {
            return None;
        }
        self.process().resource_limits.lock().cpu_signal(runtime_us)
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    /// RLIMIT_CPU 是否有限（deferred 采样据此筛选）。
    pub(in crate::task) fn cpu_limited(&self) -> bool {
        self.cpu_limit_active.get()
    }

    pub(in crate::task) fn resource_limit(
        &self,
        resource: usize,
    ) -> Option<ResourceLimit> {
        self.resource_limits.lock().get(resource)
    }
    pub(in crate::task) fn replace_resource_limit(
        &self,
        resource: usize,
        replacement: ResourceLimit,
        privileged: bool,
    ) -> Result<ResourceLimit, ResourceLimitError> {
        let mut limits = self.resource_limits.lock();
        let previous_cpu_active = limits.cpu_limit_active();
        let replacement_cpu_active =
            replacement.soft != RLIM_INFINITY || replacement.hard != RLIM_INFINITY;
        if resource == RLIMIT_CPU && replacement_cpu_active {
            // 有限 limit 可见之前先发布 true：并发采样最多多取一次锁，不会漏掉检查。
            self.cpu_limit_active.set(true);
        }
        let result = limits.replace(resource, replacement, privileged);
        if resource == RLIMIT_CPU {
            let active = if result.is_ok() {
                replacement_cpu_active
            } else {
                previous_cpu_active
            };
            self.cpu_limit_active.set(active);
        }
        result
    }
}

/// Process 的 RLIMIT_CPU 是否有限，并维护全局“有限 Process 数”。
///
/// 计数非零时 deferred 线程每秒采样一次各受限 Process 的 CPU 时间；没有受限 Process 时不产生
/// 周期唤醒。计数随标志的每次翻转与 Process 释放同步更新。
pub(in crate::task) struct CpuLimitActivity(core::sync::atomic::AtomicBool);

// OWNER: `CpuLimitActivity` 为 true 的存活 Process 数；只由 `CpuLimitActivity` 修改。
// 缺失时 deferred 线程无法判断是否需要周期采样：要么永远空转唤醒，要么漏掉 RLIMIT_CPU。
static CPU_LIMITED_PROCESSES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

impl CpuLimitActivity {
    pub(in crate::task) fn new(active: bool) -> Self {
        if active {
            CPU_LIMITED_PROCESSES.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
        }
        Self(core::sync::atomic::AtomicBool::new(active))
    }

    pub(in crate::task) fn get(&self) -> bool {
        self.0.load(core::sync::atomic::Ordering::Acquire)
    }

    pub(in crate::task) fn set(&self, active: bool) {
        use core::sync::atomic::Ordering;
        match (self.0.swap(active, Ordering::AcqRel), active) {
            (false, true) => {
                CPU_LIMITED_PROCESSES.fetch_add(1, Ordering::AcqRel);
                // deferred 线程可能正无限期停驻：让它重新计算截止时间以排定采样。
                crate::deferred::deadline_changed();
            }
            (true, false) => {
                CPU_LIMITED_PROCESSES.fetch_sub(1, Ordering::AcqRel);
            }
            _ => {}
        }
    }
}

impl Drop for CpuLimitActivity {
    fn drop(&mut self) {
        if *self.0.get_mut() {
            CPU_LIMITED_PROCESSES.fetch_sub(1, core::sync::atomic::Ordering::AcqRel);
        }
    }
}

/// 存在 RLIMIT_CPU 有限的 Process。
pub(in crate::task) fn cpu_limits_active() -> bool {
    CPU_LIMITED_PROCESSES.load(core::sync::atomic::Ordering::Acquire) != 0
}
