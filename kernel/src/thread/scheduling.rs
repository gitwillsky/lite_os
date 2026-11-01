//! 线程调度状态：运行状态机、Ready generation、CFS virtual runtime。
//!
//! # 运行状态
//!
//! ```text
//! New ──start──► Ready ──select──► Running ──block──► Blocking ──switched out──► Blocked
//!                  ▲                  │  └──preempt──► Preempting ─┐            │
//!                  └──────────────────┴─────────────── wake ◄──────┴────────────┘
//! ```
//!
//! `Blocking`/`Preempting` 表示线程已决定离开 CPU、但上下文尚未保存；唤醒在此期间到达时
//! 转为 `WakePending`，由切出后的 handoff 直接重新入队，避免丢失唤醒。

use core::num::NonZeroU64;
use core::sync::atomic::AtomicUsize;

use spin::Mutex;

use crate::execution::cpu::{self, CpuId, CpuSet};
use crate::execution::sync::{IrqMutex, TaskWaitKey, WaitResult};

const NICE_0_LOAD_SHIFT: u32 = 10;
const VRUNTIME_FRACTION_BITS: u32 = 10;
const VRUNTIME_RECIPROCAL_SHIFT: u32 = 32 - NICE_0_LOAD_SHIFT - VRUNTIME_FRACTION_BITS;
const VRUNTIME_RECIPROCAL_MASK: u64 = (1 << VRUNTIME_RECIPROCAL_SHIFT) - 1;

/// 默认权重等级（40 级中的第 20 级，权重 1024）。
pub(crate) const DEFAULT_PRIORITY: u8 = 20;
/// 权重等级数。
pub(crate) const PRIORITY_LEVELS: u8 = 40;

// CFS 权重表的 2^32/weight 预计算倒数（相邻等级权重比约 1.25）。deschedule 热路径只做乘法与移位，
// 运行时除法会显著放大 syscall/yield 密集负载的成本。下标越小权重越大。
#[rustfmt::skip]
const PRIORITY_WEIGHT_RECIPROCAL: [u32; PRIORITY_LEVELS as usize] = [
        48_388,     59_856,     76_040,     92_818,    118_348,
       147_320,    184_698,    229_616,    287_308,    360_437,
       449_829,    563_644,    704_093,    875_809,  1_099_582,
     1_376_151,  1_717_300,  2_157_191,  2_708_050,  3_363_326,
     4_194_304,  5_237_765,  6_557_202,  8_165_337, 10_153_587,
    12_820_798, 15_790_321, 19_976_592, 24_970_740, 31_350_126,
    39_045_157, 49_367_440, 61_356_676, 76_695_844, 95_443_717,
   119_304_647, 148_102_320, 186_737_708, 238_609_294, 286_331_153,
];

/// 线程可运行的逻辑 CPU 集合。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) struct CpuAffinity(CpuSet);

impl CpuAffinity {
    /// 全部 possible CPU。
    pub(crate) fn all() -> Self {
        Self(cpu::possible())
    }

    /// 是否允许在 `cpu` 上运行。
    pub(crate) fn allows(self, cpu: CpuId) -> bool {
        self.0.contains(cpu)
    }

    /// 与 active CPU 相交后的集合位图（诊断）。
    pub(crate) fn effective_bits(self) -> usize {
        (self.0 & cpu::active()).native_word()
    }
}

/// 调度器唯一权威的运行状态。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum RunState {
    New,
    Ready { cpu: CpuId, generation: u64 },
    Running { cpu: CpuId },
    Preempting { cpu: CpuId },
    Blocking { cpu: CpuId },
    Blocked,
    WakePending { cpu: CpuId },
    Exited,
}

/// 阻塞线程的唯一等待身份；唤醒者必须出示同一身份才能消费它。
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum WaitMembership {
    /// `TaskEvent`/`TaskMutex` 等同步原语的等待。
    Event(TaskWaitKey),
    /// 纯定时睡眠。
    Sleep(u64),
}

/// 状态、generation 与等待身份必须在同一 IRQ-safe 临界区转换；拆锁会允许重复入队。
pub(crate) struct SchedulingState {
    run_state: RunState,
    next_generation: u64,
    pub(crate) wait: Option<WaitMembership>,
    pub(crate) wait_result: Option<WaitResult>,
    pub(crate) cpu_affinity: CpuAffinity,
}

/// 线程的调度实体。
pub(crate) struct SchedulingEntity {
    pub(crate) state: IrqMutex<SchedulingState>,
    pub(crate) policy: Mutex<Policy>,
    /// 仅作为下次选核的亲和性提示，不发布状态。
    pub(crate) last_cpu: AtomicUsize,
}

impl SchedulingEntity {
    pub(crate) fn new(priority: u8, vruntime: u64, last_cpu: usize) -> Self {
        Self {
            state: IrqMutex::new(SchedulingState {
                run_state: RunState::New,
                next_generation: 0,
                wait: None,
                wait_result: None,
                cpu_affinity: CpuAffinity::all(),
            }),
            policy: Mutex::new(Policy::new(priority, vruntime)),
            last_cpu: AtomicUsize::new(last_cpu),
        }
    }
}

/// 已发布 `Ready`、尚未提交 per-CPU Ready 计数的线性 token；遗失会让负载投影与状态分裂。
#[must_use = "Ready transition must commit its per-CPU membership projection"]
pub(crate) struct ReadyTransition<'owner> {
    previous_cpu: Option<CpuId>,
    target_cpu: CpuId,
    generation: u64,
    committed: bool,
    owner: core::marker::PhantomData<&'owner mut SchedulingState>,
}

impl ReadyTransition<'_> {
    pub(super) fn consume(mut self) -> (Option<CpuId>, CpuId, u64) {
        self.committed = true;
        (self.previous_cpu, self.target_cpu, self.generation)
    }
}

impl Drop for ReadyTransition<'_> {
    fn drop(&mut self) {
        assert!(self.committed, "Ready state published without per-CPU membership commit");
    }
}

/// 已撤销 `Ready`、尚未撤销 per-CPU Ready 计数的线性 token。
#[must_use = "Ready retirement must commit its per-CPU membership projection"]
pub(crate) struct ReadyRetirement<'owner> {
    cpu: CpuId,
    committed: bool,
    owner: core::marker::PhantomData<&'owner mut SchedulingState>,
}

impl ReadyRetirement<'_> {
    pub(super) fn consume(mut self) -> CpuId {
        self.committed = true;
        self.cpu
    }
}

impl Drop for ReadyRetirement<'_> {
    fn drop(&mut self) {
        assert!(self.committed, "Ready state retired without per-CPU membership commit");
    }
}

impl SchedulingState {
    /// 当前运行状态。
    pub(crate) fn run_state(&self) -> RunState {
        self.run_state
    }

    /// 在不进入或离开 Ready 时替换状态；Ready 进出必须经线性 token。
    pub(crate) fn replace_non_ready_state(&mut self, state: RunState) {
        assert!(
            !matches!(self.run_state, RunState::Ready { .. })
                && !matches!(state, RunState::Ready { .. }),
            "Ready membership requires a linear transition token"
        );
        self.run_state = state;
    }

    /// 发布新的 Ready generation，使此前所有队列 entry 失效。
    pub(crate) fn transition_to_ready(&mut self, cpu: CpuId) -> ReadyTransition<'_> {
        assert!(self.cpu_affinity.allows(cpu), "Ready transition selected a disallowed CPU");
        let previous_cpu = match self.run_state {
            RunState::Ready { cpu, .. } => Some(cpu),
            RunState::New
            | RunState::Blocked
            | RunState::WakePending { .. }
            | RunState::Preempting { .. } => None,
            state => panic!("invalid Ready transition from {state:?}"),
        };
        self.next_generation = self.next_generation.wrapping_add(1);
        assert_ne!(self.next_generation, 0, "runqueue generation wrapped");
        let generation = self.next_generation;
        self.run_state = RunState::Ready { cpu, generation };
        ReadyTransition {
            previous_cpu,
            target_cpu: cpu,
            generation,
            committed: false,
            owner: core::marker::PhantomData,
        }
    }

    /// 验证队列 token 并把 Ready 转为 Running。
    pub(crate) fn transition_ready_to_running(
        &mut self,
        cpu: CpuId,
        generation: u64,
    ) -> ReadyRetirement<'_> {
        assert_eq!(
            self.run_state,
            RunState::Ready { cpu, generation },
            "selected queue token lost Ready ownership"
        );
        self.run_state = RunState::Running { cpu };
        ReadyRetirement {
            cpu,
            committed: false,
            owner: core::marker::PhantomData,
        }
    }
}

/// CFS 策略与已提交的 CPU 时间。
pub(crate) struct Policy {
    // OWNER: active slice 起点以 `time + 1` 编码进 NonZero niche；None 表示未运行。缺少 finish 时清空会重复提交。
    active_start: Option<NonZeroU64>,
    // OWNER: active slice 冻结 dispatch 时的权重等级；缺失时远端调整优先级会追溯改变已消耗时间的计费。
    active_priority: u8,
    priority: u8,
    /// Q10 微秒 vruntime；1024 单位等于默认权重的 1µs CPU 时间。
    pub(crate) vruntime: u64,
    total_runtime_us: u64,
}

impl Policy {
    fn new(priority: u8, vruntime: u64) -> Self {
        assert!(priority < PRIORITY_LEVELS);
        Self {
            active_start: None,
            active_priority: PRIORITY_LEVELS,
            priority,
            vruntime,
            total_runtime_us: 0,
        }
    }

    /// 替换权重等级；下一个 slice 生效。
    pub(crate) fn set_priority(&mut self, priority: u8) {
        assert!(priority < PRIORITY_LEVELS);
        self.priority = priority;
    }

    /// 已提交的 CPU 时间（微秒）。
    pub(crate) fn total_runtime_us(&self) -> u64 {
        self.total_runtime_us
    }

    /// 开始一个 active slice。
    pub(crate) fn begin(&mut self, now_us: u64) {
        let encoded = now_us.checked_add(1).and_then(NonZeroU64::new).expect("runtime clock exhausted");
        assert!(self.active_start.replace(encoded).is_none(), "thread dispatched with an active slice");
        assert_eq!(
            core::mem::replace(&mut self.active_priority, self.priority),
            PRIORITY_LEVELS,
            "thread dispatched with an active priority snapshot"
        );
    }

    /// 恰好一次结束 active slice 并提交时间。
    pub(crate) fn finish(&mut self, now_us: u64) {
        let start = self.active_start.take().expect("thread slice finished twice").get() - 1;
        let priority = self.active_priority();
        self.active_priority = PRIORITY_LEVELS;
        self.commit(now_us.saturating_sub(start), priority);
    }

    /// 不结束 slice 地提交 tick 前已消耗的时间。
    pub(crate) fn checkpoint(&mut self, now_us: u64) {
        let encoded = now_us.checked_add(1).and_then(NonZeroU64::new).expect("runtime clock exhausted");
        let start = self.active_start.replace(encoded).expect("checkpoint without an active slice").get() - 1;
        let priority = self.active_priority();
        self.commit(now_us.saturating_sub(start), priority);
    }

    fn active_priority(&self) -> usize {
        let priority = self.active_priority as usize;
        assert!(priority < PRIORITY_LEVELS as usize, "active slice lost its priority snapshot");
        priority
    }

    fn commit(&mut self, runtime_us: u64, priority: usize) {
        self.total_runtime_us = self.total_runtime_us.saturating_add(runtime_us);
        super::processor::account_current_cpu_runtime(runtime_us);
        let reciprocal = PRIORITY_WEIGHT_RECIPROCAL[priority] as u64;
        // Q10 vruntime = runtime * 2^20 / weight；倒数已含 2^32。先拆掉低位避免 64-bit 乘法溢出，
        // 真正超出 u64 时饱和，保持 vruntime 单调。
        let whole = (runtime_us >> VRUNTIME_RECIPROCAL_SHIFT).saturating_mul(reciprocal);
        let fraction = ((runtime_us & VRUNTIME_RECIPROCAL_MASK) * reciprocal) >> VRUNTIME_RECIPROCAL_SHIFT;
        self.vruntime = self.vruntime.saturating_add(whole.saturating_add(fraction));
    }
}

