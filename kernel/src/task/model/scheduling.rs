use super::*;
use crate::task::processor::account_current_hart_runtime;
use core::sync::atomic::Ordering;

/// @description blocked task 的唯一 wait registration membership ID。
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
}

/// @description blocked task 恢复时由唯一 wait registration 发布的结果。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WaitResult {
    Woken,
    TimedOut,
    Interrupted,
}

#[derive(Debug)]
pub(crate) struct Sched {
    /// 本次运行开始的 monotonic 时间，只在 sched mutex 内访问。
    pub(crate) last_runtime: u64,
    /// nice 值与 CFS virtual runtime。
    pub(crate) nice: i32,
    pub(crate) vruntime: u64,
    /// Thread 实际占用 CPU 的累计微秒数；procfs 只读取。
    pub(crate) total_runtime_us: u64,
    /// 所属 Process 的唯一 CPU runtime counter。
    pub(super) process_runtime_us: Arc<AtomicU64>,
}

/// @description 调度器唯一拥有和解释的 Thread 运行状态。
pub(crate) struct SchedulingEntity {
    // state/generation/wait_key 必须在一个 IRQ-safe 临界区内转换；拆锁会允许重复 enqueue。
    pub(crate) state: IrqMutex<SchedulingState>,
    pub(crate) policy: Mutex<Sched>,
    /// 只作为下次 CPU 选择的亲和性 hint，不发布 task 状态。
    pub(crate) last_cpu: AtomicUsize,
}

/// @description run state、enqueue generation 与 wait membership 的唯一权威。
#[derive(Debug)]
pub(crate) struct SchedulingState {
    pub(crate) run_state: RunState,
    pub(crate) next_generation: u64,
    pub(crate) wait: Option<WaitMembership>,
    pub(crate) wait_result: Option<WaitResult>,
}

impl SchedulingState {
    /// @description 创建新的唯一 Ready generation，并使此前所有 queue entry 失效。
    pub(crate) fn transition_to_ready(&mut self, cpu: usize) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        assert_ne!(self.next_generation, 0, "runqueue generation wrapped");
        let generation = self.next_generation;
        self.run_state = RunState::Ready { cpu, generation };
        generation
    }
}

impl Sched {
    pub(crate) fn get_dynamic_priority(&self) -> i32 {
        (20 + self.nice).clamp(0, 39)
    }

    /// @description 同时累计 Thread、Process 与 hart runtime，并推进 CFS virtual runtime。
    pub(crate) fn update_vruntime(&mut self, runtime_us: u64) {
        self.total_runtime_us = self.total_runtime_us.saturating_add(runtime_us);
        self.process_runtime_us
            .fetch_add(runtime_us, Ordering::Relaxed);
        account_current_hart_runtime(runtime_us);
        let weight = match self.get_dynamic_priority() {
            0..=9 => 4,
            10..=19 => 2,
            _ => 1,
        };
        self.vruntime += runtime_us / weight;
    }
}
