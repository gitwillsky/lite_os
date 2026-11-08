//! 每个逻辑 CPU 的调度执行状态：current、idle 上下文、本地 runqueue 与远端投递 mailbox。

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    num::NonZeroU64,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use super::Thread;
use super::scheduler::deadline::Parameters;

use super::scheduler::run_queue::{RunQueue, RunQueueEntry};
use super::scheduling::{CpuAffinity, RunState, WaitMembership};
use crate::arch::context::KernelContext;
use crate::execution::cpu::{self, CpuId, CpuSet};
use crate::execution::sync::{IrqMutex, LocalIrqGuard, LocalIrqTransfer, WaitResult};
use crate::platform;

mod admission;
mod handoff;
mod placement;
mod ready_membership;
mod ready_queue;
pub(crate) use admission::DeadlineError;
pub(super) use admission::DeadlineReservation;
pub(super) use admission::ThreadSlot;
pub(super) use handoff::{publish_pending_handoff, resume_without_switch, take_pending_handoff};
pub(super) use placement::enqueue_new;
use placement::{ready_entry, select_cpu, select_cpu_after_preemption};
use ready_membership::{commit_ready_retirement, commit_ready_transition};

/// 新的外部唤醒可以请求抢占；切出线程的回队不是新的唤醒，不能反抢刚选定的 successor。
#[derive(Clone, Copy)]
enum ReadyCause {
    Wake,
    SwitchOut,
}

/// idle 上下文被异常恢复时的 fail-stop 目标。
fn idle_return() -> ! {
    panic!("idle context returned unexpectedly");
}

/// 仅由所属 CPU 可变访问的调度状态。
pub(super) struct Processor {
    cpu_id: CpuId,
    pub(super) current: Option<Arc<Thread>>,
    idle_context: KernelContext,
    runqueue: RunQueue,
    pending_handoff: Option<handoff::PendingHandoff>,
    deferred_reap: Option<Arc<Thread>>,
}

impl Processor {
    fn new(cpu_id: CpuId) -> Self {
        let mut idle_context = KernelContext::zero_init();
        idle_context.set_resume_target(idle_return);
        Self {
            cpu_id,
            current: None,
            idle_context,
            runqueue: RunQueue::new(),
            pending_handoff: None,
            deferred_reap: None,
        }
    }

    /// 当前 CPU idle 上下文的稳定地址。
    pub(super) fn idle_context_ptr(&mut self) -> *mut KernelContext {
        &mut self.idle_context
    }

    fn add_ready_entry(&mut self, entry: RunQueueEntry) {
        ready_queue::add_ready_entry(self, entry)
    }

    /// 选择下一个线程并发布为 current。
    pub(super) fn select(&mut self) -> Option<Arc<Thread>> {
        ready_queue::select(self)
    }

    /// 撤销 current 与 Running 负载计数。
    pub(super) fn take_current(&mut self) -> Option<Arc<Thread>> {
        ready_queue::update_floor(self);
        let current = self.current.take()?;
        current_per_cpu()
            .reschedule_requested
            .store(false, Ordering::Relaxed);
        let previous = current_per_cpu()
            .running_entries
            .fetch_sub(1, Ordering::Relaxed);
        assert_eq!(previous, 1, "running load counter lost current ownership");
        Some(current)
    }

    /// 把远端 mailbox 中的线程转入本地 runqueue。
    pub(super) fn drain_inbound(&mut self) -> bool {
        ready_queue::drain_inbound(self)
    }

    fn defer_reap(&mut self, thread: Arc<Thread>) {
        assert!(
            self.deferred_reap.is_none(),
            "deferred reap slot must be drained before another thread exits"
        );
        self.deferred_reap = Some(thread);
    }
}

struct PerCpuProcessor {
    local: UnsafeCell<MaybeUninit<Processor>>,
    initialized: AtomicBool,
    // OWNER: owner CPU 在 IRQ-disabled borrow 内独占 `local`；嵌套借用会同时存在两个 `&mut Processor`。
    borrowed: AtomicBool,
    // OWNER: Ready membership 数的投影；Relaxed 过期值只影响瞬时选核提示。
    ready_entries: AtomicUsize,
    // OWNER: Running membership 数的投影；缺失会让选核把忙 CPU 当成空闲。
    running_entries: AtomicUsize,
    // OWNER: 远端可发布的 reschedule 请求，本 CPU 返回用户态前唯一消费。
    reschedule_requested: AtomicBool,
    // OWNER: 本 CPU 单调前进的虚拟时间下限；New 不得插队，真实睡眠者只保留有界信用。
    placement_vruntime: AtomicU64,
    // OWNER: 已提交量与 active 区间是一个复合状态，IRQ-safe 锁同时发表；远端读取不能混合两代。
    // 只锁固定整数操作，不取 policy/queue 锁、不分配；拆成独立原子会在 commit 间隙重复计入旧区间。
    busy: IrqMutex<CpuRuntime>,
    // OWNER: 只有本 CPU 读写，记录自上次放宽亲和偏置以来的抢占次数（见 `placement_policy`）。
    preemptions_since_rotation: AtomicU32,
    // timer softirq 可远端投递；IRQ-safe 锁防止打断本 CPU drain 后再入。
    inbound: IrqMutex<Inbound>,
}

/// 目标 CPU 的跨核投递和两份已在创建线程时准备好的替换存储。
struct Inbound {
    entries: VecDeque<RunQueueEntry>,
    spare_entries: VecDeque<RunQueueEntry>,
    spare_runqueue: RunQueue,
}

impl PerCpuProcessor {
    /// 只有所属 CPU 在 IRQ-masked scheduler 事务中前移下限；远端只读取。
    /// 旧睡眠者重新进入队列不得倒退该时钟，否则下一次唤醒又能领取无限旧额度。
    fn advance_placement_floor(&self, floor: u64) {
        if floor > self.placement_vruntime.load(Ordering::Relaxed) {
            self.placement_vruntime.store(floor, Ordering::Release);
        }
    }

    fn new() -> Self {
        Self {
            local: UnsafeCell::new(MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
            borrowed: AtomicBool::new(false),
            ready_entries: AtomicUsize::new(0),
            running_entries: AtomicUsize::new(0),
            reschedule_requested: AtomicBool::new(false),
            placement_vruntime: AtomicU64::new(0),
            busy: IrqMutex::new(CpuRuntime::default()),
            preemptions_since_rotation: AtomicU32::new(0),
            inbound: IrqMutex::new(Inbound {
                entries: VecDeque::new(),
                spare_entries: VecDeque::new(),
                spare_runqueue: RunQueue::new(),
            }),
        }
    }
}

// SAFETY: `local` 只由所属 CPU 在关中断时访问；远端只触及原子投影与 inbound IrqMutex。
unsafe impl Sync for PerCpuProcessor {}

/// 本 CPU 实际执行区间的唯一计费投影；时间是纳秒，零起点用 NonZero niche 编码。
#[derive(Default)]
struct CpuRuntime {
    committed_ns: u64,
    active_from: Option<NonZeroU64>,
}
fn runtime_anchor(now_ns: u64) -> NonZeroU64 {
    now_ns
        .checked_add(1)
        .and_then(NonZeroU64::new)
        .expect("runtime clock exhausted")
}
/// 持有计费锁后读取边界时间，同时发布已提交量和后续区间，返回实际边界。
/// `continuing` 表示 checkpoint，否则本次 finish 结束 active 区间。
/// `sample` 必须是无阻塞的单调时钟读取，延迟到锁内调用；提前取时间会让跨 CPU 快照倒退。
pub(super) fn commit_runtime_interval(
    cpu_index: usize,
    start_ns: u64,
    continuing: bool,
    sample: impl FnOnce() -> u64,
) -> u64 {
    let mut busy = processor_at(cpu_index).busy.lock();
    assert_eq!(
        busy.active_from.map(|start| start.get() - 1),
        Some(start_ns),
        "CPU and policy runtime intervals diverged"
    );
    let now_ns = sample();
    let elapsed = now_ns
        .checked_sub(start_ns)
        .expect("runtime clock moved backwards");
    busy.committed_ns = busy.committed_ns.saturating_add(elapsed);
    busy.active_from = continuing.then(|| runtime_anchor(now_ns));
    now_ns
}

/// 只有当前 CPU 在 dispatch 边界调用；取时钟与发布 active 区间同锁，不与读者交换顺序。
pub(super) fn begin_runtime_interval(sample: impl FnOnce() -> u64) -> u64 {
    let mut busy = current_per_cpu().busy.lock();
    assert!(
        busy.active_from.is_none(),
        "CPU dispatched overlapping runtime intervals"
    );
    let now_ns = sample();
    busy.active_from = Some(runtime_anchor(now_ns));
    now_ns
}

/// 本 CPU 进入 idle：没有未提交的运行区间，也不再需要时间片截止。
pub(super) fn enter_idle() {
    assert!(
        current_per_cpu().busy.lock().active_from.is_none(),
        "idle entered with uncommitted runtime"
    );
    with_current_processor(|processor| rearm_slice(processor));
}

/// 按本 CPU 当前状态重新武装时间片截止：有运行线程且 runqueue 有竞争者才需要，否则撤销。
///
/// 调用者在 IRQ 屏蔽下已更新 floor（过期 root 已丢弃）。
pub(super) fn rearm_slice(processor: &Processor) {
    let budget = processor
        .current
        .as_ref()
        .and_then(|thread| thread.scheduling.policy.lock().budget_limit());
    let replenishment = processor.runqueue.next_replenishment();
    let deadline = match (budget, replenishment) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    crate::execution::timer::set_scheduler_deadline(
        processor.current.is_some() && budget.is_none() && processor.runqueue.has_eligible(),
        deadline,
    );
}

/// 每个 CPU 的累计执行时间（纳秒），按逻辑 CPU 序。
///
/// # Errors
/// 分配失败返回 `Err(())`。
pub(crate) fn cpu_busy_snapshot() -> Result<Vec<u64>, ()> {
    let slots = &PROCESSOR_TOPOLOGY.wait().slots;
    let mut snapshot = Vec::new();
    snapshot.try_reserve_exact(slots.len()).map_err(|_| ())?;
    snapshot.extend(slots.iter().map(|slot| {
        let busy = slot.processor.busy.lock();
        let active = busy.active_from.map_or(0, |start| {
            crate::execution::timer::get_time_ns().saturating_sub(start.get() - 1)
        });
        busy.committed_ns.saturating_add(active)
    }));
    Ok(snapshot)
}

struct ProcessorSlot {
    cpu_id: CpuId,
    processor: PerCpuProcessor,
}

struct ProcessorTopology {
    slots: Box<[ProcessorSlot]>,
    // OWNER: admission 串行发布所有 CPU 的备用存储。普通入队不取该锁；没有它会出现
    // 并发创建时部分 CPU 仍只有旧容量，已接纳的线程无法在 IRQ-off 路径投递。
    admitted_capacity: IrqMutex<usize>,
    // OWNER: 每个 ThreadSlot 恰好计入一次（包括尚未发布和仍被旧 generation 保活的线程）。
    // Drop 可发生在队列锁内，所以不能反向获取 admission 锁。
    thread_slots: AtomicUsize,
    // OWNER: 各 CPU 已承诺的 Q32 截止密度；凭据归还也取此锁，不取线程/队列锁。
    // 缺失会使并发准入超售。同一事务扣旧添新，分配/唤醒/队列投递均在锁外。
    deadline_bandwidth: IrqMutex<Box<[u64]>>,
}

// OWNER: 每个平台 CPU 的调度状态。
static PROCESSOR_TOPOLOGY: spin::Once<ProcessorTopology> = spin::Once::new();

/// 按逻辑 CPU 序构造调度状态。队列 backing 由创建线程的事务按需准备。
pub(super) fn init_topology() {
    assert!(
        PROCESSOR_TOPOLOGY.get().is_none(),
        "processor topology initialized twice"
    );
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(cpu::count())
        .expect("processor topology allocation failed");
    for cpu_id in cpu::possible().iter() {
        slots.push(ProcessorSlot {
            cpu_id,
            processor: PerCpuProcessor::new(),
        });
    }
    PROCESSOR_TOPOLOGY.call_once(|| ProcessorTopology {
        deadline_bandwidth: IrqMutex::new(alloc::vec![0; cpu::count()].into_boxed_slice()),
        slots: slots.into_boxed_slice(),
        admitted_capacity: IrqMutex::new(0),
        thread_slots: AtomicUsize::new(0),
    });
}

// OWNER: 初始 placement 的轮转游标；Relaxed，只用于分散扫描起点。
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);

fn processor_at(index: usize) -> &'static PerCpuProcessor {
    &PROCESSOR_TOPOLOGY.wait().slots[index].processor
}

fn current_slot() -> &'static ProcessorSlot {
    &PROCESSOR_TOPOLOGY.wait().slots[cpu::current_id().index()]
}

fn current_per_cpu() -> &'static PerCpuProcessor {
    processor_at(cpu::current_id().index())
}

/// 在关中断期间独占访问当前 CPU 的调度状态。
///
/// 1. 关中断保证同 CPU trap handler 不能在借用存活时再次借用；
/// 2. borrowed flag 拒绝闭包内嵌套调用；
/// 3. panic 不展开，只在闭包正常返回后释放 flag。
pub(super) fn with_current_processor<R>(f: impl FnOnce(&mut Processor) -> R) -> R {
    let _irq = LocalIrqGuard::disable();
    let slot = current_slot();
    let processor = &slot.processor;
    assert!(
        !processor.borrowed.swap(true, Ordering::Relaxed),
        "nested current Processor borrow"
    );
    if !processor.initialized.load(Ordering::Relaxed) {
        // SAFETY: 只有当前 CPU 能到达自己的 slot.local，borrowed flag 证明没有其他引用。
        unsafe {
            (*processor.local.get()).write(Processor::new(slot.cpu_id));
        }
        processor.initialized.store(true, Ordering::Relaxed);
    }
    // SAFETY: initialized 证明已构造；IRQ guard 与 borrowed flag 证明这是唯一 `&mut Processor`。
    let local = unsafe { (*processor.local.get()).assume_init_mut() };
    let result = f(local);
    processor.borrowed.store(false, Ordering::Relaxed);
    result
}

/// 把已退出线程在其内核栈上的最后 owner 移交当前 CPU，切到 idle 后释放。
pub(super) fn defer_reap(thread: Arc<Thread>) {
    with_current_processor(|processor| processor.defer_reap(thread));
}

/// 在 idle 栈上释放 deferred 退出线程。
pub(super) fn reap_deferred() {
    let thread = with_current_processor(|processor| processor.deferred_reap.take());
    drop(thread);
}

/// 标记当前 CPU 返回用户态前需要重新调度。
pub(crate) fn request_reschedule() {
    current_per_cpu()
        .reschedule_requested
        .store(true, Ordering::Release);
}

/// 消费当前 CPU 的 reschedule 请求。
pub(crate) fn take_reschedule() -> bool {
    current_per_cpu()
        .reschedule_requested
        .swap(false, Ordering::AcqRel)
}

fn publish_reschedule_at(cpu_id: CpuId) {
    let target = &PROCESSOR_TOPOLOGY.wait().slots[cpu_id.index()];
    target
        .processor
        .reschedule_requested
        .store(true, Ordering::Release);
    if target.cpu_id != cpu::current_id() {
        platform::send_ipi(CpuSet::singleton(target.cpu_id))
            .expect("platform IPI failed for remote reschedule");
    }
}

/// 当前 CPU 在安全点接管远端 Ready，并仅按本批新的有效 entry 裁决抢占。
///
/// 只借用本 CPU 状态，IRQ guard 覆盖整个事务；高 vruntime/stale entry 不触发抢占。
/// 不分配或复制 Ready membership；调用者已消费 RUNQUEUE deferred bit。
pub(super) fn dispatch_inbound_ready() {
    let _irq = LocalIrqGuard::disable();
    let preempt = with_current_processor(Processor::drain_inbound);
    if preempt {
        request_reschedule();
    }
}

/// 时间片到期时，本 CPU 有竞争者才请求重新调度。
pub(super) fn request_slice_reschedule() {
    let running = with_current_processor(|processor| {
        ready_queue::replenish(processor);
        let running = processor.current.is_some();
        if !running {
            rearm_slice(processor);
        }
        running
    });
    if running {
        request_reschedule();
    }
}

/// 让正在某 CPU 上执行的线程尽快经过一次调度点（kill/kick 后返回用户态前被观察到）。
pub(super) fn request_thread_reschedule(thread: &Arc<Thread>) {
    let cpu = match thread.scheduling.state.lock().run_state() {
        RunState::Running { cpu }
        | RunState::Preempting { cpu }
        | RunState::Blocking { cpu }
        | RunState::WakePending { cpu } => Some(cpu),
        RunState::New | RunState::Ready { .. } | RunState::Blocked | RunState::Exited => None,
    };
    if let Some(cpu) = cpu {
        publish_reschedule_at(cpu);
    }
}

/// 把当前 Running 线程转为 Preempting。
pub(super) fn begin_preempt_current(thread: &Arc<Thread>) {
    let source_cpu = cpu::current_id();
    let current = with_current_processor(Processor::take_current)
        .expect("preemption requires current thread");
    assert!(Arc::ptr_eq(&current, thread));
    let mut scheduling = thread.scheduling.state.lock();
    match scheduling.run_state() {
        RunState::Running { cpu } => {
            assert_eq!(cpu, source_cpu, "preemption source CPU diverged");
            scheduling.replace_non_ready_state(RunState::Preempting { cpu: source_cpu });
        }
        state => panic!("preemption source lost running ownership: {state:?}"),
    }
}

fn deliver_ready_entry(cpu_id: CpuId, entry: RunQueueEntry, cause: ReadyCause) {
    ready_queue::deliver_ready_entry(cpu_id, entry, cause);
}

/// 原子替换线程 affinity，并迁移位于已禁止 CPU 的 Ready membership。
///
/// 正在已禁止 CPU 上运行的线程被请求重新调度：抢占后的 CPU 选择遵守新 affinity。
pub(super) fn replace_affinity(thread: &Arc<Thread>, affinity: CpuAffinity) -> Result<(), ()> {
    let mut replacement = None;
    let mut running_elsewhere = None;
    {
        let mut scheduling = thread.scheduling.state.lock();
        if thread
            .scheduling
            .policy
            .lock()
            .deadline_cpu()
            .is_some_and(|cpu| !affinity.allows(cpu))
        {
            return Err(());
        }
        scheduling.cpu_affinity = affinity;
        match scheduling.run_state() {
            RunState::Ready { cpu, .. } if !affinity.allows(cpu) => {
                let target = select_cpu(thread, affinity);
                placement::rebase_for_migration(thread, cpu, target);
                replacement = Some((
                    target,
                    commit_ready_transition(scheduling.transition_to_ready(target)),
                ));
            }
            RunState::Running { cpu } if !affinity.allows(cpu) => running_elsewhere = Some(cpu),
            _ => {}
        }
    }
    if let Some((cpu, generation)) = replacement {
        deliver_ready_entry(
            cpu,
            ready_entry(thread.clone(), generation),
            ReadyCause::Wake,
        );
    }
    if let Some(cpu) = running_elsewhere {
        publish_reschedule_at(cpu);
    }
    Ok(())
}

/// 消费精确等待身份并使线程可运行。
///
/// # Returns
/// 身份匹配并被本次调用消费时为 `true`；重复或过期唤醒为 `false`。
pub(super) fn wake_waiting(
    thread: Arc<Thread>,
    expected: WaitMembership,
    result: WaitResult,
    prefer_current_cpu: bool,
) -> bool {
    let ready = {
        let mut scheduling = thread.scheduling.state.lock();
        if scheduling.wait != Some(expected) {
            return false;
        }
        scheduling.wait = None;
        assert!(scheduling.wait_result.is_none());
        scheduling.wait_result = Some(result);
        match scheduling.run_state() {
            RunState::Blocking { cpu } => {
                scheduling.replace_non_ready_state(RunState::WakePending { cpu });
                None
            }
            RunState::Blocked => {
                let local = cpu::current_id();
                let target = if prefer_current_cpu
                    && cpu::is_active(local)
                    && scheduling.cpu_affinity.allows(local)
                    && thread.scheduling.policy.lock().deadline_cpu().is_none()
                {
                    local
                } else {
                    select_cpu(&thread, scheduling.cpu_affinity)
                };
                // last_cpu 在当前 Blocked transaction 内是已保存 slice 的坐标 owner：
                // begin_runtime 写入它后才可能进入 Blocking/Blocked，下一次 Running 前不会改写。
                let source = cpu::id_at(thread.scheduling.last_cpu.load(Ordering::Relaxed))
                    .expect("blocked thread has no execution CPU");
                placement::rebase_for_migration(&thread, source, target);
                // 实际睡眠才获得有界 I/O 响应额度；早到 wake 不曾离开 CPU，不走此分支。
                {
                    let mut policy = thread.scheduling.policy.lock();
                    policy.place_waking(
                        processor_at(target.index())
                            .placement_vruntime
                            .load(Ordering::Acquire),
                        crate::execution::timer::scheduler_quantum_ns(),
                    );
                    policy.activate_deadline(crate::execution::timer::get_time_ns);
                }
                let generation = commit_ready_transition(scheduling.transition_to_ready(target));
                Some((target, generation))
            }
            RunState::Exited => None,
            state => panic!("wait membership attached to invalid state {state:?}"),
        }
    };
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(cpu, ready_entry(thread, generation), ReadyCause::Wake);
    }
    true
}

/// 当前等待身份（若阻塞中）；kill 用它打断可中断等待。
pub(super) fn waiting_membership(thread: &Thread) -> Option<WaitMembership> {
    thread.scheduling.state.lock().wait
}

/// 在 next 线程或 idle continuation 上完成 outgoing 线程的 Blocking/WakePending/Preempting 转换。
pub(super) fn finish_deschedule(thread: &Arc<Thread>) {
    let cpu = cpu::current_id();
    let ready = {
        let mut scheduling = thread.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::Blocking { cpu: owner } => {
                assert_eq!(owner, cpu, "blocking thread returned on another CPU");
                scheduling.voluntary_switches += 1;
                scheduling.replace_non_ready_state(RunState::Blocked);
                None
            }
            RunState::WakePending { cpu: owner } => {
                assert_eq!(owner, cpu, "wake-pending thread returned on another CPU");
                scheduling.voluntary_switches += 1;
                let target = if scheduling.cpu_affinity.allows(cpu)
                    && thread
                        .scheduling
                        .policy
                        .lock()
                        .deadline_cpu()
                        .is_none_or(|reserved| reserved == cpu)
                {
                    cpu
                } else {
                    select_cpu(thread, scheduling.cpu_affinity)
                };
                placement::rebase_for_migration(thread, owner, target);
                Some((
                    target,
                    commit_ready_transition(scheduling.transition_to_ready(target)),
                    ReadyCause::Wake,
                ))
            }
            RunState::Preempting { cpu: owner } => {
                assert_eq!(owner, cpu, "preempting thread returned on another CPU");
                scheduling.involuntary_switches += 1;
                let target = select_cpu_after_preemption(thread, scheduling.cpu_affinity);
                placement::rebase_for_migration(thread, owner, target);
                Some((
                    target,
                    commit_ready_transition(scheduling.transition_to_ready(target)),
                    ReadyCause::SwitchOut,
                ))
            }
            _ => None,
        }
    };
    if let Some((target, generation, cause)) = ready {
        deliver_ready_entry(target, ready_entry(thread.clone(), generation), cause);
    }
}

/// 修改策略并使旧 Ready 键失效；state -> policy -> bandwidth，任何账本路径都不反向取线程锁。
/// 运行体先按原配置 checkpoint（允许远端），再替换预算；目标 CPU 通过 IPI 重排并重装截止。
pub(in crate::thread) fn replace_deadline(
    thread: &Arc<Thread>,
    parameters: Option<Parameters>,
) -> Result<(), DeadlineError> {
    let mut ready = None;
    let mut reschedule = None;
    {
        let mut state = thread.scheduling.state.lock();
        if state.run_state() == RunState::Exited || (parameters.is_some() && thread.is_killed()) {
            return Err(DeadlineError::Terminated);
        }
        let mut policy = thread.scheduling.policy.lock();
        if !policy
            .replace_deadline(parameters, state.cpu_affinity)
            .map_err(|()| DeadlineError::Capacity)?
        {
            return Ok(());
        }
        if parameters.is_none() {
            let index = match state.run_state() {
                RunState::Ready { cpu, .. } | RunState::Running { cpu } => cpu.index(),
                _ => thread.scheduling.last_cpu.load(Ordering::Relaxed),
            };
            policy.vruntime = policy.vruntime.max(
                processor_at(index)
                    .placement_vruntime
                    .load(Ordering::Acquire),
            );
        }
        match state.run_state() {
            RunState::Ready { cpu, .. } => {
                let target = policy.deadline_cpu().unwrap_or(cpu);
                drop(policy);
                placement::rebase_for_migration(thread, cpu, target);
                let generation = commit_ready_transition(state.transition_to_ready(target));
                ready = Some((target, generation));
            }
            RunState::Running { cpu } => reschedule = Some(cpu),
            _ => {}
        }
    }
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(
            cpu,
            ready_entry(thread.clone(), generation),
            ReadyCause::Wake,
        );
    }
    if let Some(cpu) = reschedule {
        publish_reschedule_at(cpu);
    }
    Ok(())
}
