//! Linux 线程的执行状态：等待身份、停止、亲和性与 Linux 调度属性。
//!
//! 就绪队列、抢占与 CPU 时间记账属于内核调度器；linuxd 只记录 Linux 语义需要的状态：
//!
//! 1. 线程阻塞时发布精确的 [`WaitMembership`]，唤醒者比较并消费它，再唤醒承载线程的 parker；
//! 2. group stop 把运行中的线程标为 `StopPending`（它在下一个安全点停驻），把已阻塞的线程标为
//!    `Stopped`（醒来后继续停驻），直到 SIGCONT；
//! 3. nice 映射为内核权重等级，亲和性下发到承载线程。

use super::*;
use crate::cpu::{self, CpuSet};
use crate::sync::WaitResult;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 线程可运行的逻辑 CPU 集合。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(in crate::task) struct CpuAffinity(CpuSet);

impl CpuAffinity {
    /// 全部 CPU。
    pub(in crate::task) fn all_possible() -> Self {
        Self(cpu::possible())
    }

    /// 把用户 CPU 位图收敛到存在的 CPU；交集为空返回 `None`。
    pub(in crate::task) fn from_user_bits(bits: usize) -> Option<Self> {
        let effective = CpuSet::from_native_word(bits) & cpu::active();
        (!effective.is_empty()).then_some(Self(effective))
    }

    /// 生效的 CPU 位图。
    pub(in crate::task) fn effective_bits(self) -> usize {
        (self.0 & cpu::active()).native_word()
    }
}

/// 阻塞线程的唯一等待身份。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WaitMembership {
    Deadline(u64),
    Child,
    Vfork(usize),
    Futex(u64),
    Console(u64),
    Signal(u64),
    Pipe(u64),
    AdvisoryLock(u64),
    Poll(u64),
    TaskWait(crate::sync::TaskWaitKey),
}

impl WaitMembership {
    /// 计入负载均值的不可中断等待（Linux TASK_UNINTERRUPTIBLE）。
    fn contributes_to_load(self) -> bool {
        match self {
            Self::Vfork(_) => true,
            Self::TaskWait(key) => key.contributes_to_load(),
            _ => false,
        }
    }
}

/// group stop 解除后线程回到的状态。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum StopResume {
    /// 尚未首次运行。
    New,
    /// 继续执行。
    Runnable,
    /// 仍在等待。
    Blocked,
}

/// 线程的执行状态。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RunState {
    New,
    Running,
    Blocked,
    /// 运行中被要求停止：在下一个安全点停驻。
    StopPending,
    Stopped {
        resume: StopResume,
    },
    Exited,
}

// OWNER: 不可中断等待中的线程数（`/proc/loadavg` 与 `procs_blocked`）；由 SchedulingState 的转换维护。
static BLOCKED: AtomicUsize = AtomicUsize::new(0);
// OWNER: 处于 Running/StopPending 的线程数（`procs_running`）。
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// 不可中断等待中的线程数。
pub(in crate::task) fn uninterruptible_task_count() -> usize {
    BLOCKED.load(Ordering::Relaxed)
}

/// 可运行线程数。
pub(in crate::task) fn runnable_task_count() -> usize {
    RUNNING.load(Ordering::Relaxed)
}

/// 线程执行状态的唯一权威。
#[derive(Debug)]
pub(crate) struct SchedulingState {
    run_state: RunState,
    pub(crate) wait: Option<WaitMembership>,
    pub(crate) wait_result: Option<WaitResult>,
    // FLAG: 本线程当前计入 BLOCKED；缺失时转换无法知道是否需要归还计数。
    counted_blocked: bool,
    pub(in crate::task) cpu_affinity: CpuAffinity,
}

impl SchedulingState {
    pub(super) fn new(cpu_affinity: CpuAffinity) -> Self {
        Self {
            run_state: RunState::New,
            wait: None,
            wait_result: None,
            counted_blocked: false,
            cpu_affinity,
        }
    }

    pub(in crate::task) fn run_state(&self) -> RunState {
        self.run_state
    }

    /// 替换执行状态，同时维护可运行与不可中断计数。
    pub(in crate::task) fn set_run_state(&mut self, state: RunState) {
        let was_running = matches!(self.run_state, RunState::Running | RunState::StopPending);
        let running = matches!(state, RunState::Running | RunState::StopPending);
        match (was_running, running) {
            (false, true) => {
                RUNNING.fetch_add(1, Ordering::Relaxed);
            }
            (true, false) => {
                RUNNING.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let blocked = matches!(
            state,
            RunState::Blocked
                | RunState::Stopped {
                    resume: StopResume::Blocked
                }
        ) && self.wait.is_some_and(WaitMembership::contributes_to_load);
        if blocked != self.counted_blocked {
            if blocked {
                BLOCKED.fetch_add(1, Ordering::Relaxed);
            } else {
                BLOCKED.fetch_sub(1, Ordering::Relaxed);
            }
            self.counted_blocked = blocked;
        }
        self.run_state = state;
    }

    /// 不可中断等待（`/proc` 的 D 状态）。
    pub(in crate::task) fn is_uninterruptible(&self) -> bool {
        matches!(self.run_state, RunState::Blocked)
            && self.wait.is_some_and(WaitMembership::contributes_to_load)
    }

    /// 空闲等待（`/proc` 的 I 状态）。
    pub(in crate::task) fn is_idle_wait(&self) -> bool {
        matches!(self.run_state, RunState::Blocked)
            && matches!(self.wait, Some(WaitMembership::TaskWait(key)) if key.idle())
    }
}

impl Drop for SchedulingState {
    fn drop(&mut self) {
        self.set_run_state(RunState::Exited);
    }
}

/// Linux 调度属性。
#[derive(Debug, Clone)]
pub(crate) struct Sched {
    pub(crate) nice: i32,
    reset_on_fork: bool,
    io_priority: u16,
}

impl Sched {
    pub(super) fn new(nice: i32) -> Self {
        Self {
            nice,
            reset_on_fork: false,
            io_priority: 0,
        }
    }

    /// Linux `sched_fork`：reset-on-fork 时负 nice 归零，子线程清除该标志。
    pub(super) fn forked(&self) -> Self {
        Self {
            nice: if self.reset_on_fork {
                self.nice.max(0)
            } else {
                self.nice
            },
            reset_on_fork: false,
            io_priority: self.io_priority,
        }
    }

    pub(in crate::task) fn reset_on_fork(&mut self, replacement: Option<bool>) -> bool {
        let previous = self.reset_on_fork;
        if let Some(replacement) = replacement {
            self.reset_on_fork = replacement;
        }
        previous
    }

    /// 查询或替换 nice（-20..=19）。
    pub(in crate::task) fn nice(&mut self, replacement: Option<i32>) -> i32 {
        let previous = self.nice;
        if let Some(replacement) = replacement {
            assert!((-20..=19).contains(&replacement));
            self.nice = replacement;
        }
        previous
    }

    pub(in crate::task) fn io_priority(&mut self, replacement: Option<u16>) -> u16 {
        let previous = self.io_priority;
        if let Some(replacement) = replacement {
            self.io_priority = replacement;
        }
        previous
    }

    /// `/proc` 的动态优先级。
    pub(crate) fn get_dynamic_priority(&self) -> i32 {
        (20 + self.nice).clamp(0, 39)
    }

    /// 内核权重等级（0..40，小者权重大）。
    pub(crate) fn kernel_priority(&self) -> u32 {
        (20 + self.nice).clamp(0, 39) as u32
    }
}

/// 线程的调度实体。
pub(crate) struct SchedulingEntity {
    pub(crate) state: Mutex<SchedulingState>,
    pub(crate) policy: Mutex<Sched>,
    /// 最近一次观察到的 CPU（`/proc` 的 processor 字段）；只是提示。
    pub(crate) last_cpu: AtomicUsize,
    // OWNER: 线程退出时冻结的自身 CPU 时间（微秒），`u64::MAX` 表示尚未退出；承载线程结束后
    // 内核记账不再可查，缺失时僵尸 leader 的 `/proc/<pid>/stat` utime/stime 归零。
    pub(crate) retired_runtime_us: core::sync::atomic::AtomicU64,
}

impl SchedulingEntity {
    pub(super) fn try_new(
        policy: Sched,
        cpu_affinity: CpuAffinity,
        last_cpu: usize,
    ) -> Result<Arc<Self>, alloc::alloc::AllocError> {
        Arc::try_new(Self {
            state: Mutex::new(SchedulingState::new(cpu_affinity)),
            policy: Mutex::new(policy),
            last_cpu: AtomicUsize::new(last_cpu),
            retired_runtime_us: core::sync::atomic::AtomicU64::new(u64::MAX),
        })
    }
}

impl TaskControlBlock {
    /// 快照 Thread 创建时刻、调度属性与 CPU 时间（`/proc/<pid>/task/<tid>/stat`）。
    ///
    /// # Returns
    ///
    /// `(start_time_us, nice, priority, runtime_us)`；CPU 时间来自承载线程的内核记账，退出后为冻结值。
    pub(in crate::task) fn thread_statistics(&self) -> (u64, i32, i32, u64) {
        let (nice, priority) = {
            let policy = self.scheduling.policy.lock();
            (policy.nice, policy.get_dynamic_priority())
        };
        let start_time_us = if !self.is_kernel_thread() && self.tid() == self.tgid() {
            self.process().start_time_us
        } else {
            self.execution.start_time_us
        };
        let runtime_us = match self.scheduling.retired_runtime_us.load(Ordering::Acquire) {
            u64::MAX => self.thread_cpu_time().0,
            retired => retired,
        };
        (start_time_us, nice, priority, runtime_us)
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    /// leader 的 `(nice, priority, last_cpu, 退出时冻结的 CPU 时间)`，供僵尸 leader 的投影。
    pub(in crate::task) fn process_scheduler_snapshot(&self) -> (i32, i32, usize, u64) {
        let scheduler = self.leader_scheduler.lock().clone();
        let policy = scheduler.policy.lock();
        (
            policy.nice,
            policy.get_dynamic_priority(),
            scheduler.last_cpu.load(Ordering::Relaxed),
            match scheduler.retired_runtime_us.load(Ordering::Acquire) {
                u64::MAX => 0,
                retired => retired,
            },
        )
    }
}
