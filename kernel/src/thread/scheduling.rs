//! 线程调度状态：运行状态机、Ready generation、CFS nanosecond virtual runtime。
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
const VRUNTIME_RECIPROCAL_SHIFT: u32 = 32 - NICE_0_LOAD_SHIFT;
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

    /// 与 possible CPU 相交后的集合；交集为空返回 `None`（线程将无处运行）。
    pub(crate) fn restricted_to(cpus: CpuSet) -> Option<Self> {
        let cpus = cpus & cpu::possible();
        (!cpus.is_empty()).then_some(Self(cpus))
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
    /// 线程因阻塞离开 CPU 的次数（含阻塞发布后、上下文保存前即被唤醒的情形）。
    pub(crate) voluntary_switches: u64,
    /// 线程仍可运行却被抢占或主动让出的次数。
    pub(crate) involuntary_switches: u64,
    // OWNER: 成功解决的 restricted 缺页及其中的 pager 等待；与调度状态同锁投影，
    // 不保存页驻留状态。缺失时用户态执行服务无法观察由 VMO 直接解决的缺页。
    pub(crate) restricted_faults: u64,
    pub(crate) restricted_pager_waits: u64,
}

/// 线程的调度实体。
pub(crate) struct SchedulingEntity {
    pub(crate) state: IrqMutex<SchedulingState>,
    pub(crate) policy: Mutex<Policy>,
    /// 仅作为下次选核的亲和性提示，不发布状态。
    /// begin_runtime 唯一写入的上次执行 CPU。选核时只是缓存提示；在 Blocked 的 state guard 内，
    /// 前次 Running→Blocking→Blocked 的锁交接保证它是已保存 policy 的坐标 owner。
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
                voluntary_switches: 0,
                involuntary_switches: 0,
                restricted_faults: 0,
                restricted_pager_waits: 0,
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
    pub(super) fn consume_ready_projection_parts(mut self) -> (Option<CpuId>, CpuId, u64) {
        self.committed = true;
        (self.previous_cpu, self.target_cpu, self.generation)
    }
}

impl Drop for ReadyTransition<'_> {
    fn drop(&mut self) {
        assert!(
            self.committed,
            "Ready state published without per-CPU membership commit"
        );
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
    pub(super) fn consume_ready_projection_cpu(mut self) -> CpuId {
        self.committed = true;
        self.cpu
    }
}

impl Drop for ReadyRetirement<'_> {
    fn drop(&mut self) {
        assert!(
            self.committed,
            "Ready state retired without per-CPU membership commit"
        );
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
        assert!(
            self.cpu_affinity.allows(cpu),
            "Ready transition selected a disallowed CPU"
        );
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

/// 公平/CBS 策略与已提交的 CPU 时间。
pub(crate) struct Policy {
    // OWNER: active slice 起点以 `time + 1` 编码进 NonZero niche；None 表示未运行。缺少 finish 时清空会重复提交。
    active_start: Option<NonZeroU64>,
    // OWNER: active slice 冻结 dispatch 时的权重等级；缺失时远端调整优先级会追溯改变已消耗时间的计费。
    active_priority: u8,
    // OWNER: active 区间所在 CPU；远端配置更改必须提交该 CPU 的计费，不能记到调用 CPU。
    active_cpu: usize,
    // OWNER: CBS 状态与唯一准入凭据同属策略锁，配置更改前先结算旧预算。
    deadline: Option<(
        super::scheduler::deadline::Cbs,
        super::processor::DeadlineReservation,
    )>,
    priority: u8,
    /// 加权纳秒 vruntime；默认权重下 1 ns CPU 时间对应一个单位。
    pub(crate) vruntime: u64,
    total_runtime_ns: u64,
    // OWNER: 跨核 rebase 时记录已计费总量。缺失时按固定抢占节拍迁移会反复选中刚迁入的线程。
    runtime_at_migration_ns: u64,
    // FLAG: 线程当前在 restricted mode；之后提交的时间同时计入 `restricted_runtime_ns`。
    // 缺失时服务无法区分受限代码与自身代码消耗的时间（例如兼容层的 user/system 时间）。
    restricted: bool,
    restricted_runtime_ns: u64,
}

impl Policy {
    fn new(priority: u8, vruntime: u64) -> Self {
        assert!(priority < PRIORITY_LEVELS);
        Self {
            active_start: None,
            active_priority: PRIORITY_LEVELS,
            active_cpu: usize::MAX,
            deadline: None,
            priority,
            vruntime,
            total_runtime_ns: 0,
            runtime_at_migration_ns: 0,
            restricted: false,
            restricted_runtime_ns: 0,
        }
    }

    /// 切换计时桶：先把当前 slice 已消耗的时间按旧模式提交。
    pub(crate) fn set_restricted(&mut self, sample: impl FnOnce() -> u64, restricted: bool) {
        if self.active_start.is_some() {
            self.checkpoint(sample);
        }
        self.restricted = restricted;
    }

    /// 替换权重等级；下一个 slice 生效。
    pub(crate) fn set_priority(&mut self, priority: u8) {
        assert!(priority < PRIORITY_LEVELS);
        self.priority = priority;
    }

    /// 原子替换 CBS 配置/准入凭据；准入失败保留原策略，成功前结算旧 active 区间。
    pub(super) fn replace_deadline(
        &mut self,
        parameters: Option<super::scheduler::deadline::Parameters>,
        affinity: CpuAffinity,
    ) -> Result<bool, ()> {
        use super::scheduler::deadline::Cbs;
        if parameters
            == self
                .deadline
                .as_ref()
                .map(|(server, _)| server.parameters())
        {
            return Ok(false);
        }
        let replacement = parameters
            .map(|parameters| {
                super::processor::DeadlineReservation::replace(
                    self.deadline.as_mut().map(|(_, reservation)| reservation),
                    parameters,
                    affinity,
                )
                .map(|reservation| (parameters, reservation))
            })
            .transpose()?;
        if self.active_start.is_some() {
            self.checkpoint(crate::execution::timer::get_time_ns);
        }
        self.deadline = replacement.map(|(parameters, reservation)| {
            (
                Cbs::new(parameters, crate::execution::timer::get_time_ns()),
                reservation,
            )
        });
        Ok(true)
    }

    /// 退出后不再承诺带宽；不等待 stale 队列/观察者最后一个 Arc 退休。
    pub(super) fn retire_deadline(&mut self) {
        self.deadline = None;
    }

    /// 显式 yield 放弃本周期剩余额度；必须在 finish 后执行，避免把原 active 区间重复记作债务。
    pub(super) fn forfeit_deadline(&mut self) {
        assert!(
            self.active_start.is_none(),
            "yield budget before runtime commit"
        );
        if let Some((server, _)) = &mut self.deadline {
            server.forfeit();
        }
    }

    pub(super) fn deadline_cpu(&self) -> Option<CpuId> {
        self.deadline
            .as_ref()
            .map(|(_, reservation)| reservation.cpu())
    }

    /// 真正唤醒时执行 CBS 激活测试；抢占和普通 yield 不能重置预算。
    pub(super) fn activate_deadline(&mut self, sample: impl FnOnce() -> u64) {
        if let Some((server, _)) = &mut self.deadline {
            server.wake(sample());
        }
    }

    /// Ready 的不可变排序键；只有已到达的补充时刻才可推进预算。
    pub(super) fn ready_key(
        &mut self,
        sample: impl FnOnce() -> u64,
    ) -> super::scheduler::deadline::ReadyKey {
        use super::scheduler::deadline::ReadyKey;
        if let Some((server, _)) = &mut self.deadline {
            server.replenish(sample());
            ReadyKey::Deadline {
                deadline: server.deadline(),
                not_before: server.not_before(),
            }
        } else {
            ReadyKey::Fair(self.vruntime)
        }
    }

    /// 当前执行体的排序与预算硬截止；不会刷新唤醒信用或遗漏无竞争者时的节流。
    pub(super) fn budget_limit(&self) -> Option<u64> {
        self.deadline.as_ref().map(|(server, _)| {
            server.budget_limit(
                self.active_start
                    .map_or_else(crate::execution::timer::get_time_ns, |start| {
                        start.get() - 1
                    }),
            )
        })
    }

    /// 截至 `now_ns` 的 CPU 时间与 restricted 时间：已提交量加当前 active slice 的存量。
    ///
    /// 没有周期 tick 提交 checkpoint，长时间连续运行的线程只在切换时提交；观察者必须读取时叠加存量，
    /// 否则它们看到的运行时间会停在上一次切换。
    pub(crate) fn runtime_at(&self, now_ns: u64) -> (u64, u64) {
        let active = self
            .active_start
            .map_or(0, |start| now_ns.saturating_sub(start.get() - 1));
        let restricted = if self.restricted { active } else { 0 };
        (
            self.total_runtime_ns.saturating_add(active),
            self.restricted_runtime_ns.saturating_add(restricted),
        )
    }

    /// 本次 CPU 驻留以来已计费的执行时间，用于并列负载的跨核轮转。
    pub(super) fn runtime_on_cpu_ns(&self) -> u64 {
        self.total_runtime_ns - self.runtime_at_migration_ns
    }

    /// 限制睡眠后可保留的虚拟时间优势，不把未运行的睡眠记为 CPU 使用。
    ///
    /// # Parameters
    /// - `floor`: 目标 CPU 已发布、单调前进的加权纳秒下限。
    /// - `credit_ns`: 默认权重归一化的最大睡眠额度（纳秒）；较早的历史优势在这里截断。
    pub(super) fn place_waking(&mut self, floor: u64, credit_ns: u64) {
        self.vruntime = self.vruntime.max(floor.saturating_sub(credit_ns));
    }

    /// 换算到目标 CPU 的虚拟时间坐标；只改变 placement，不计入实际运行时间。
    ///
    /// # Parameters
    /// - `source_floor`: source CPU 的单调虚拟时间下限。
    /// - `target_floor`: target CPU 的单调虚拟时间下限。
    ///
    /// 正/负 lag 分别保留已消耗的服务债务与睡眠优势；绝对坐标在 u64 边界饱和。
    /// # Panics
    /// active slice 尚未结束时拒绝迁移，不能把正在计费的区间换到另一时钟。
    pub(super) fn rebase(&mut self, source_floor: u64, target_floor: u64) {
        assert!(
            self.active_start.is_none(),
            "cannot migrate an active slice"
        );
        self.runtime_at_migration_ns = self.total_runtime_ns;
        self.vruntime = if self.vruntime >= source_floor {
            target_floor.saturating_add(self.vruntime - source_floor)
        } else {
            target_floor.saturating_sub(source_floor - self.vruntime)
        };
    }

    /// 开始 active slice；时钟回调延迟到本 CPU 计费锁内读取，不接受提前缓存的时间戳。
    pub(crate) fn begin(&mut self, sample: impl FnOnce() -> u64) {
        assert!(
            self.active_start.is_none(),
            "thread dispatched with an active slice"
        );
        assert_eq!(
            self.active_priority, PRIORITY_LEVELS,
            "thread dispatched with an active priority snapshot"
        );
        let now_ns = super::processor::begin_runtime_interval(sample);
        self.active_start = Some(runtime_start(now_ns));
        self.active_priority = self.priority;
        self.active_cpu = crate::execution::cpu::current_id().index();
    }

    /// 恰好一次结束 active slice；CPU 投影与本策略使用同一个锁内采样边界。
    pub(crate) fn finish(&mut self, sample: impl FnOnce() -> u64) {
        let start = self
            .active_start
            .take()
            .expect("thread slice finished twice")
            .get()
            - 1;
        let priority = self.active_priority();
        let now_ns =
            super::processor::commit_runtime_interval(self.active_cpu, start, false, sample);
        self.active_priority = PRIORITY_LEVELS;
        self.active_cpu = usize::MAX;
        self.commit(now_ns - start, priority);
    }

    /// 不结束 slice 地提交当前边界前的纳秒；锁内取时间，读者不能先观察到一个更晚的旧区间。
    pub(crate) fn checkpoint(&mut self, sample: impl FnOnce() -> u64) -> u64 {
        let start = self
            .active_start
            .expect("checkpoint without an active slice")
            .get()
            - 1;
        let priority = self.active_priority();
        let now_ns =
            super::processor::commit_runtime_interval(self.active_cpu, start, true, sample);
        self.active_start = Some(runtime_start(now_ns));
        self.commit(now_ns - start, priority);
        now_ns
    }

    /// 为新的唤醒裁决计费；尚未开始的切换目标只提供已提交坐标，不伪造 active slice。
    pub(super) fn checkpoint_for_wakeup(
        &mut self,
        sample: impl FnOnce() -> u64,
    ) -> super::scheduler::deadline::ReadyKey {
        let now = self.active_start.map(|_| self.checkpoint(sample));
        self.ready_key(|| now.unwrap_or_else(crate::execution::timer::get_time_ns))
    }

    fn active_priority(&self) -> usize {
        let priority = self.active_priority as usize;
        assert!(
            priority < PRIORITY_LEVELS as usize,
            "active slice lost its priority snapshot"
        );
        priority
    }

    fn commit(&mut self, runtime_ns: u64, priority: usize) {
        self.total_runtime_ns = self.total_runtime_ns.saturating_add(runtime_ns);
        if self.restricted {
            self.restricted_runtime_ns = self.restricted_runtime_ns.saturating_add(runtime_ns);
        }
        if let Some((server, _)) = &mut self.deadline {
            server.charge(runtime_ns);
            return;
        }
        let reciprocal = PRIORITY_WEIGHT_RECIPROCAL[priority] as u64;
        // vruntime(ns) = runtime_ns * 1024 / weight；倒数已含 2^32。拆低位避免 64-bit 乘法溢出，
        // 真正超出 u64 时饱和，保持 vruntime 单调。
        let whole = (runtime_ns >> VRUNTIME_RECIPROCAL_SHIFT).saturating_mul(reciprocal);
        let fraction =
            ((runtime_ns & VRUNTIME_RECIPROCAL_MASK) * reciprocal) >> VRUNTIME_RECIPROCAL_SHIFT;
        self.vruntime = self.vruntime.saturating_add(whole.saturating_add(fraction));
    }
}

fn runtime_start(now_ns: u64) -> NonZeroU64 {
    now_ns
        .checked_add(1)
        .and_then(NonZeroU64::new)
        .expect("runtime clock exhausted")
}
