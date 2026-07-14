use super::*;
use crate::{arch::hart, task::processor::account_current_hart_runtime};
use core::{num::NonZeroU64, sync::atomic::Ordering};

const NICE_0_LOAD_SHIFT: u32 = 10;
const VRUNTIME_FRACTION_BITS: u32 = 10;
const VRUNTIME_RECIPROCAL_SHIFT: u32 = 32 - NICE_0_LOAD_SHIFT - VRUNTIME_FRACTION_BITS;
const VRUNTIME_RECIPROCAL_MASK: u64 = (1 << VRUNTIME_RECIPROCAL_SHIFT) - 1;

// Linux v7.1 sched_prio_to_wmult[]：2^32/weight 的预计算 reciprocal。直接保留固定表可让
// deschedule 热路径只做乘法与移位；运行时除法会显著放大 syscall/yield 密集负载的成本。
#[rustfmt::skip]
const NICE_TO_WEIGHT_RECIPROCAL: [u32; 40] = [
    // -20 .. -16
        48_388,     59_856,     76_040,     92_818,    118_348,
    // -15 .. -11
       147_320,    184_698,    229_616,    287_308,    360_437,
    // -10 ..  -6
       449_829,    563_644,    704_093,    875_809,  1_099_582,
    //  -5 ..  -1
     1_376_151,  1_717_300,  2_157_191,  2_708_050,  3_363_326,
    //   0 ..   4
     4_194_304,  5_237_765,  6_557_202,  8_165_337, 10_153_587,
    //   5 ..   9
    12_820_798, 15_790_321, 19_976_592, 24_970_740, 31_350_126,
    //  10 ..  14
    39_045_157, 49_367_440, 61_356_676, 76_695_844, 95_443_717,
    //  15 ..  19
   119_304_647, 148_102_320, 186_737_708, 238_609_294, 286_331_153,
];

/// @description 以紧凑 topology index 表示 Thread 可运行的 logical CPU 集合。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(in crate::task) struct CpuAffinity(usize);

impl CpuAffinity {
    fn active_bits() -> usize {
        hart::states()
            .iter()
            .enumerate()
            .fold(0, |bits, (cpu, state)| {
                if state.is_active() {
                    bits | (1usize << cpu)
                } else {
                    bits
                }
            })
    }

    /// @description 构造包含动态 topology 全部 possible CPU 的初始 affinity。
    ///
    /// @return 非空 logical CPU mask；CPU 尚未 active 不影响初始继承集合。
    pub(in crate::task) fn all_possible() -> Self {
        let count = hart::states().len();
        assert!(count != 0 && count <= usize::BITS as usize);
        Self(usize::MAX >> (usize::BITS as usize - count))
    }

    /// @description 将 userspace logical CPU mask 收敛到当前 active scheduler topology。
    ///
    /// @param bits Linux CPU index bitmap。
    /// @return 至少保留一个 active CPU 时返回规范化 affinity，否则返回 `None`。
    pub(in crate::task) fn from_user_bits(bits: usize) -> Option<Self> {
        let effective = bits & Self::active_bits();
        (effective != 0).then_some(Self(effective))
    }

    /// @description 投影当前 active CPU 上实际生效的 logical mask。
    ///
    /// @return stored affinity 与 active topology 的交集。
    pub(in crate::task) fn effective_bits(self) -> usize {
        self.0 & Self::active_bits()
    }

    /// @description 判断 raw hart 是否对应 affinity 中允许的 logical CPU。
    ///
    /// @param hart_id DTB raw hart ID。
    /// @return hart 存在且对应 logical bit 已设置时返回 `true`。
    pub(in crate::task) fn allows_hart(self, hart_id: usize) -> bool {
        hart::hart_index(hart_id).is_some_and(|cpu| self.allows_cpu(cpu))
    }

    /// @description 判断紧凑 topology index 是否包含在 affinity 中。
    ///
    /// @param cpu 零基 logical CPU index。
    /// @return 对应 bit 已设置时返回 `true`。
    pub(in crate::task) fn allows_cpu(self, cpu: usize) -> bool {
        self.0 & (1usize << cpu) != 0
    }
}

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
    /// OWNER: SchedulingEntity policy lock 唯一拥有 active CPU slice 起点；None 表示未运行。
    /// 起点以 `time + 1` 编码进 NonZero niche；缺少 finish 时清空会让同一 slice 被重复提交。
    active_runtime_start: Option<NonZeroU64>,
    // OWNER: active slice 冻结 dispatch 时的 nice index；缺失该快照会让远端 setpriority
    // 把已经消耗的 CPU 时间按新权重追溯计费。40 是 inactive sentinel。
    active_priority: u8,
    /// nice 值与 CFS virtual runtime。
    pub(crate) nice: i32,
    /// Q10 微秒 vruntime；1024 units 等于 nice 0 的 1µs CPU runtime。
    pub(crate) vruntime: u64,
    // OWNER: policy lock 独占 Linux reset-on-fork policy；缺失该 flag 会让
    // sched_getscheduler 的返回值与 child 实际继承的 nice 语义分裂。
    reset_on_fork: bool,
    // OWNER: SchedulingEntity policy lock 独占 per-Thread I/O priority；若只让 syscall
    // 返回默认值，htop 显示会与后续 set 操作分裂且 fork 无法继承真实 policy。
    io_priority: u16,
    /// Thread 已提交的 CPU runtime；只由该 policy 更新和投影。
    total_runtime_us: u64,
    /// 所属 Process 的唯一 CPU runtime counter。
    process_runtime_us: Arc<AtomicU64>,
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
    /// OWNER: SchedulingState 与 run state 同锁拥有 Thread affinity；拆锁会让 Ready migration
    /// 与并发 wake 各自发布到不同 CPU，使旧 mask 下的 entry 再次运行。
    pub(in crate::task) cpu_affinity: CpuAffinity,
}

impl SchedulingState {
    /// @description 构造没有 runqueue/wait membership 的新 Thread 调度状态。
    ///
    /// @param cpu_affinity 创建者继承或初始化的唯一 CPU 集合。
    /// @return `New` 状态且 generation 为零的 scheduling owner。
    pub(super) fn new(cpu_affinity: CpuAffinity) -> Self {
        Self {
            run_state: RunState::New,
            next_generation: 0,
            wait: None,
            wait_result: None,
            cpu_affinity,
        }
    }

    /// @description 创建新的唯一 Ready generation，并使此前所有 queue entry 失效。
    pub(crate) fn transition_to_ready(&mut self, cpu: usize) -> u64 {
        assert!(
            self.cpu_affinity.allows_hart(cpu),
            "Ready transition selected a disallowed CPU"
        );
        self.next_generation = self.next_generation.wrapping_add(1);
        assert_ne!(self.next_generation, 0, "runqueue generation wrapped");
        let generation = self.next_generation;
        self.run_state = RunState::Ready { cpu, generation };
        generation
    }

    /// @description 判断 Thread 是否仍在 affinity 排除的 CPU 上持有执行/切出 ownership。
    ///
    /// @return Running 或尚未切回 idle stack 的过渡状态位于禁止 CPU 时返回 `true`。
    pub(in crate::task) fn executes_outside_affinity(&self) -> bool {
        let cpu = match self.run_state {
            RunState::Running { cpu }
            | RunState::Preempting { cpu }
            | RunState::Blocking { cpu }
            | RunState::WakePending { cpu }
            | RunState::StopPending { cpu, .. } => Some(cpu),
            RunState::New
            | RunState::Ready { .. }
            | RunState::Blocked
            | RunState::Stopped { .. }
            | RunState::Exited => None,
        };
        cpu.is_some_and(|cpu| !self.cpu_affinity.allows_hart(cpu))
    }
}

impl Sched {
    /// @description 创建尚未运行、累计时间为零的 Thread scheduling policy。
    ///
    /// @param nice 继承或初始化后的 Linux nice 值。
    /// @param vruntime 由 placement/fork policy 选择的初始 virtual runtime。
    /// @param process_runtime_us 所属 Process 的唯一聚合 CPU runtime owner。
    /// @return inactive 且 total runtime 为零的 policy。
    /// @errors 无错误。
    pub(super) fn new(nice: i32, vruntime: u64, process_runtime_us: Arc<AtomicU64>) -> Self {
        Self {
            active_runtime_start: None,
            active_priority: NICE_TO_WEIGHT_RECIPROCAL.len() as u8,
            nice,
            vruntime,
            reset_on_fork: false,
            io_priority: 0,
            total_runtime_us: 0,
            process_runtime_us,
        }
    }

    /// @description 按 Linux sched_fork 语义派生 child 的独立 scheduling policy。
    ///
    /// @param process_runtime_us child 所属 Process 的唯一聚合 CPU runtime owner。
    /// @return 继承 vruntime；reset-on-fork 生效时负 nice 归零且 child 清除该 flag。
    /// @errors 无错误。
    pub(super) fn forked(&self, process_runtime_us: Arc<AtomicU64>) -> Self {
        let reset = self.reset_on_fork;
        Self {
            active_runtime_start: None,
            active_priority: NICE_TO_WEIGHT_RECIPROCAL.len() as u8,
            nice: if reset { self.nice.max(0) } else { self.nice },
            vruntime: self.vruntime,
            reset_on_fork: false,
            io_priority: self.io_priority,
            total_runtime_us: 0,
            process_runtime_us,
        }
    }

    /// @description 查询或替换当前唯一支持的 `SCHED_OTHER` reset-on-fork 属性。
    ///
    /// @param replacement `None` 只查询；`Some` 原子替换 flag。
    /// @return 修改前的 reset-on-fork 值。
    /// @errors 无错误。
    pub(in crate::task) fn reset_on_fork(&mut self, replacement: Option<bool>) -> bool {
        let previous = self.reset_on_fork;
        if let Some(replacement) = replacement {
            self.reset_on_fork = replacement;
        }
        previous
    }

    /// @description 查询或替换唯一 policy owner 中的 Linux nice 值。
    ///
    /// @param replacement `None` 只查询；`Some` 必须已经规范化到 -20..19。
    /// @return 修改前的 nice 值。
    /// @panics replacement 越出 Linux nice 范围时 panic。
    pub(in crate::task) fn nice(&mut self, replacement: Option<i32>) -> i32 {
        let previous = self.nice;
        if let Some(replacement) = replacement {
            assert!((-20..=19).contains(&replacement));
            self.nice = replacement;
        }
        previous
    }

    /// @description 查询或替换当前 Thread 的 Linux encoded I/O priority。
    ///
    /// @param replacement None 只查询；Some 已由 syscall seam 校验 class/data encoding。
    /// @return 修改前的 encoded `IOPRIO_PRIO_VALUE`。
    pub(in crate::task) fn io_priority(&mut self, replacement: Option<u16>) -> u16 {
        let previous = self.io_priority;
        if let Some(replacement) = replacement {
            self.io_priority = replacement;
        }
        previous
    }

    pub(crate) fn get_dynamic_priority(&self) -> i32 {
        (20 + self.nice).clamp(0, 39)
    }

    /// @description 开始一个尚未提交的 active CPU slice。
    ///
    /// @param start_time_us monotonic CPU dispatch 时刻。
    /// @return 无返回值；slice 起点由 policy lock 唯一发布。
    /// @panics 前一个 slice 尚未 finish，或 monotonic 微秒计数耗尽时 panic。
    pub(in crate::task) fn begin_runtime(&mut self, start_time_us: u64) {
        let encoded = start_time_us
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .expect("runtime clock exhausted");
        assert!(
            self.active_runtime_start.replace(encoded).is_none(),
            "task dispatched with an active runtime slice"
        );
        let priority = self.get_dynamic_priority() as u8;
        assert_eq!(
            core::mem::replace(&mut self.active_priority, priority),
            NICE_TO_WEIGHT_RECIPROCAL.len() as u8,
            "task dispatched with an active priority snapshot"
        );
    }

    /// @description 恰好一次结束 active CPU slice，并累计 Thread、Process、hart 与 vruntime。
    ///
    /// @param end_time_us monotonic deschedule 时刻。
    /// @return 无返回值；全部 runtime owner 已同步推进。
    /// @panics 没有 active slice，表示 caller 重复提交或绕过 begin 时 panic。
    pub(in crate::task) fn finish_runtime(&mut self, end_time_us: u64) {
        let start_time_us = self
            .active_runtime_start
            .take()
            .expect("task runtime slice finished twice")
            .get()
            - 1;
        let priority = self.checked_active_priority();
        self.active_priority = NICE_TO_WEIGHT_RECIPROCAL.len() as u8;
        self.commit_runtime(end_time_us.saturating_sub(start_time_us), priority);
    }

    /// @description 在不结束 active slice 的前提下提交 timer tick 前已消耗的 CPU runtime。
    ///
    /// @param checkpoint_us monotonic timer deferred-work 时刻。
    /// @return 无返回值；Thread、Process、hart runtime 与 vruntime 同步推进，dispatch weight 保持冻结。
    /// @panics 没有 active slice，或 monotonic 微秒计数耗尽时 panic。
    pub(in crate::task) fn checkpoint_runtime(&mut self, checkpoint_us: u64) {
        // 1. 原子地把 active 起点推进到 checkpoint，后续 finish 只提交剩余增量。
        let encoded = checkpoint_us
            .checked_add(1)
            .and_then(NonZeroU64::new)
            .expect("runtime clock exhausted");
        let start_time_us = self
            .active_runtime_start
            .replace(encoded)
            .expect("runtime checkpoint without an active slice")
            .get()
            - 1;
        // 2. checkpoint 不结束 dispatch，必须继续使用同一 nice 权重快照。
        let priority = self.checked_active_priority();
        // 3. 所有累计 owner 一次推进，避免恢复 tick context switch 才能刷新 /proc/stat。
        self.commit_runtime(checkpoint_us.saturating_sub(start_time_us), priority);
    }

    fn checked_active_priority(&self) -> usize {
        let priority = self.active_priority as usize;
        assert!(
            priority < NICE_TO_WEIGHT_RECIPROCAL.len(),
            "active slice lost its priority snapshot"
        );
        priority
    }

    fn active_runtime_delta(&self, now_us: u64) -> u64 {
        let start_time_us = self
            .active_runtime_start
            .expect("running task has no active runtime slice")
            .get()
            - 1;
        now_us.saturating_sub(start_time_us)
    }

    /// 同时累计 Thread、Process 与 hart runtime，并推进 CFS virtual runtime。
    fn commit_runtime(&mut self, runtime_us: u64, priority: usize) {
        self.total_runtime_us = self.total_runtime_us.saturating_add(runtime_us);
        self.process_runtime_us
            .fetch_add(runtime_us, Ordering::Relaxed);
        account_current_hart_runtime(runtime_us);
        let reciprocal = NICE_TO_WEIGHT_RECIPROCAL[priority] as u64;
        // 1. Q10 vruntime 需要 runtime * 2^20 / weight；Linux reciprocal 已含 2^32。
        // 2. 先拆掉低 12 位，避免 64-bit 乘法溢出而不引入 deschedule 热路径除法。
        // 3. 任一可表示的结果精确重组；真正超出 u64 时饱和，保持 vruntime 单调。
        let whole = (runtime_us >> VRUNTIME_RECIPROCAL_SHIFT).saturating_mul(reciprocal);
        let fraction =
            ((runtime_us & VRUNTIME_RECIPROCAL_MASK) * reciprocal) >> VRUNTIME_RECIPROCAL_SHIFT;
        self.vruntime = self.vruntime.saturating_add(whole.saturating_add(fraction));
    }
}

impl TaskControlBlock {
    /// @description 快照 Thread 创建时刻、调度属性与已累计 CPU runtime。
    ///
    /// @param active_now_us calling Thread 传入本次 monotonic 时刻以包含尚未提交的 active slice；
    /// 其他 Thread 传入 `None`，保持跨 hart 最多一个 scheduler tick 的 bounded-stale 读取。
    /// @return `(start_time_us, nice, priority, runtime_us)`；读取不修改任何 owner。
    pub(in crate::task) fn thread_statistics(
        &self,
        active_now_us: Option<u64>,
    ) -> (u64, i32, i32, u64) {
        let policy = self.scheduling.policy.lock();
        let active_runtime_us =
            active_now_us.map_or(0, |now_us| policy.active_runtime_delta(now_us));
        (
            self.thread.start_time_us,
            policy.nice,
            policy.get_dynamic_priority(),
            policy.total_runtime_us.saturating_add(active_runtime_us),
        )
    }

    /// @description 快照 calling Thread 与所属 Process 的 scheduler CPU runtime。
    ///
    /// @param now_us 本次查询共用的 monotonic 微秒时刻。
    /// @return `(process_runtime_us, thread_runtime_us)`；均包含 calling Thread 尚未提交的 active slice。
    /// @panics calling Thread 当前没有 active slice，表示 syscall 绕过 scheduler running ownership。
    pub(crate) fn cpu_runtime_snapshot(&self, now_us: u64) -> (u64, u64) {
        let policy = self.scheduling.policy.lock();
        let active_runtime_us = policy.active_runtime_delta(now_us);
        let thread_runtime_us = policy.total_runtime_us.saturating_add(active_runtime_us);
        let process_runtime_us = self
            .process
            .cpu_runtime_us
            .load(Ordering::Relaxed)
            .saturating_add(active_runtime_us);
        (process_runtime_us, thread_runtime_us)
    }
}
