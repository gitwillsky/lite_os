//! 每个逻辑 CPU 的调度执行状态：current、idle 上下文、本地 runqueue 与远端投递 mailbox。

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use super::Thread;
use super::scheduler::cfs_scheduler::{CfsRunQueue, RunQueueEntry};
use super::scheduling::{CpuAffinity, RunState, WaitMembership};
use crate::arch::context::KernelContext;
use crate::execution::cpu::{self, CpuId, CpuSet};
use crate::execution::sync::{IrqMutex, LocalIrqGuard, LocalIrqTransfer, WaitResult};
use crate::platform;

mod handoff;
mod placement;
mod ready_membership;
mod ready_queue;
pub(super) use handoff::{publish_pending_handoff, resume_without_switch, take_pending_handoff};
pub(super) use placement::enqueue_new;
use placement::{ready_entry, select_cpu, select_cpu_after_preemption};
use ready_membership::{commit_ready_retirement, commit_ready_transition};

/// idle 上下文被异常恢复时的 fail-stop 目标。
fn idle_return() -> ! {
    panic!("idle context returned unexpectedly");
}

/// 仅由所属 CPU 可变访问的调度状态。
pub(super) struct Processor {
    cpu_id: CpuId,
    pub(super) current: Option<Arc<Thread>>,
    idle_context: KernelContext,
    runqueue: CfsRunQueue,
    pending_handoff: Option<handoff::PendingHandoff>,
    deferred_reap: Option<Arc<Thread>>,
}

impl Processor {
    fn new(cpu_id: CpuId, queue_capacity: usize) -> Self {
        let mut idle_context = KernelContext::zero_init();
        idle_context.set_resume_target(idle_return);
        Self {
            cpu_id,
            current: None,
            idle_context,
            runqueue: CfsRunQueue::try_with_capacity(queue_capacity)
                .expect("scheduler runqueue allocation failed"),
            pending_handoff: None,
            deferred_reap: None,
        }
    }

    /// 当前 CPU idle 上下文的稳定地址。
    pub(super) fn idle_context_ptr(&mut self) -> *mut KernelContext {
        &mut self.idle_context
    }

    fn add_ready_entry(&mut self, entry: RunQueueEntry) -> bool {
        ready_queue::add_ready_entry(self, entry)
    }

    /// 选择下一个线程并发布为 current。
    pub(super) fn select(&mut self) -> Option<Arc<Thread>> {
        ready_queue::select(self)
    }

    /// 撤销 current 与 Running 负载计数。
    pub(super) fn take_current(&mut self) -> Option<Arc<Thread>> {
        let current = self.current.take()?;
        let previous = current_per_cpu()
            .running_entries
            .fetch_sub(1, Ordering::Relaxed);
        assert_eq!(previous, 1, "running load counter lost current ownership");
        Some(current)
    }

    /// 把远端 mailbox 中的线程转入本地 runqueue。
    pub(super) fn drain_inbound(&mut self) {
        ready_queue::drain_inbound(self);
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
    // OWNER: 本 CPU 已提交的线程运行时间。
    busy_us: AtomicU64,
    // OWNER: 只有本 CPU 读写，记录自上次放宽亲和偏置以来的抢占次数（见 `placement_policy`）。
    preemptions_since_rotation: AtomicU32,
    // timer softirq 可远端投递；IRQ-safe 锁防止打断本 CPU drain 后再入。
    inbound: IrqMutex<VecDeque<RunQueueEntry>>,
    queue_capacity: usize,
}

impl PerCpuProcessor {
    /// 只有所属 CPU 在 IRQ-masked scheduler 事务中前移下限；远端只读取。
    /// 旧睡眠者重新进入队列不得倒退该时钟，否则下一次唤醒又能领取无限旧额度。
    fn advance_placement_floor(&self, floor: u64) {
        if floor > self.placement_vruntime.load(Ordering::Relaxed) {
            self.placement_vruntime.store(floor, Ordering::Release);
        }
    }

    fn new(queue_capacity: usize) -> Self {
        let mut inbound = VecDeque::new();
        inbound
            .try_reserve_exact(queue_capacity)
            .expect("scheduler inbound allocation failed");
        Self {
            local: UnsafeCell::new(MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
            borrowed: AtomicBool::new(false),
            ready_entries: AtomicUsize::new(0),
            running_entries: AtomicUsize::new(0),
            reschedule_requested: AtomicBool::new(false),
            placement_vruntime: AtomicU64::new(0),
            busy_us: AtomicU64::new(0),
            preemptions_since_rotation: AtomicU32::new(0),
            inbound: IrqMutex::new(inbound),
            queue_capacity,
        }
    }
}

// SAFETY: `local` 只由所属 CPU 在关中断时访问；远端只触及原子投影与 inbound IrqMutex。
unsafe impl Sync for PerCpuProcessor {}

pub(super) fn account_current_cpu_runtime(runtime_us: u64) {
    current_per_cpu()
        .busy_us
        .fetch_add(runtime_us, Ordering::Relaxed);
}

/// 每个 CPU 的累计忙碌时间（微秒），按逻辑 CPU 序。
///
/// # Errors
/// 分配失败返回 `Err(())`。
pub(crate) fn cpu_busy_snapshot() -> Result<Vec<u64>, ()> {
    let slots = &PROCESSOR_TOPOLOGY.wait().slots;
    let mut snapshot = Vec::new();
    snapshot.try_reserve_exact(slots.len()).map_err(|_| ())?;
    snapshot.extend(
        slots
            .iter()
            .map(|slot| slot.processor.busy_us.load(Ordering::Relaxed)),
    );
    Ok(snapshot)
}

struct ProcessorSlot {
    cpu_id: CpuId,
    processor: PerCpuProcessor,
}

struct ProcessorTopology {
    slots: Box<[ProcessorSlot]>,
}

// OWNER: 每个平台 CPU 的调度状态。
static PROCESSOR_TOPOLOGY: spin::Once<ProcessorTopology> = spin::Once::new();

/// 按逻辑 CPU 序构造调度状态；队列容量按“物理页最多容纳的 kernel stack 数”预留，运行期入队不分配。
pub(super) fn init_topology() {
    assert!(
        PROCESSOR_TOPOLOGY.get().is_none(),
        "processor topology initialized twice"
    );
    let stack_pages = crate::memory::KERNEL_STACK_SIZE / crate::memory::PAGE_SIZE;
    let queue_capacity = crate::memory::frame_statistics()
        .capacity_pages
        .div_ceil(stack_pages);
    assert!(
        queue_capacity != 0,
        "physical memory cannot host one thread stack"
    );
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(cpu::count())
        .expect("processor topology allocation failed");
    for cpu_id in cpu::possible().iter() {
        slots.push(ProcessorSlot {
            cpu_id,
            processor: PerCpuProcessor::new(queue_capacity),
        });
    }
    PROCESSOR_TOPOLOGY.call_once(|| ProcessorTopology {
        slots: slots.into_boxed_slice(),
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
            (*processor.local.get()).write(Processor::new(slot.cpu_id, processor.queue_capacity));
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

/// tick 到来时，本 CPU 有竞争者才请求重新调度。
pub(super) fn request_tick_reschedule() {
    let slot = current_per_cpu();
    if slot.running_entries.load(Ordering::Relaxed) != 0
        && slot.ready_entries.load(Ordering::Relaxed) != 0
    {
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

fn deliver_ready_entry(cpu_id: CpuId, entry: RunQueueEntry) {
    ready_queue::deliver_ready_entry(cpu_id, entry);
}

/// 原子替换线程 affinity，并迁移位于已禁止 CPU 的 Ready membership。
///
/// 正在已禁止 CPU 上运行的线程被请求重新调度：抢占后的 CPU 选择遵守新 affinity。
pub(super) fn replace_affinity(thread: &Arc<Thread>, affinity: CpuAffinity) {
    let mut replacement = None;
    let mut running_elsewhere = None;
    {
        let mut scheduling = thread.scheduling.state.lock();
        scheduling.cpu_affinity = affinity;
        match scheduling.run_state() {
            RunState::Ready { cpu, .. } if !affinity.allows(cpu) => {
                let target = select_cpu(thread, affinity);
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
        deliver_ready_entry(cpu, ready_entry(thread.clone(), generation));
    }
    if let Some(cpu) = running_elsewhere {
        publish_reschedule_at(cpu);
    }
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
                {
                    local
                } else {
                    select_cpu(&thread, scheduling.cpu_affinity)
                };
                // 实际睡眠才获得有界 I/O 响应额度；早到 wake 不曾离开 CPU，不走此分支。
                thread.scheduling.policy.lock().place_waking(
                    processor_at(target.index())
                        .placement_vruntime
                        .load(Ordering::Acquire),
                    crate::execution::timer::scheduler_quantum_ns() / 1000,
                );
                let generation = commit_ready_transition(scheduling.transition_to_ready(target));
                Some((target, generation))
            }
            RunState::Exited => None,
            state => panic!("wait membership attached to invalid state {state:?}"),
        }
    };
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(cpu, ready_entry(thread, generation));
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
                let target = if scheduling.cpu_affinity.allows(cpu) {
                    cpu
                } else {
                    select_cpu(thread, scheduling.cpu_affinity)
                };
                Some((
                    target,
                    commit_ready_transition(scheduling.transition_to_ready(target)),
                ))
            }
            RunState::Preempting { cpu: owner } => {
                assert_eq!(owner, cpu, "preempting thread returned on another CPU");
                scheduling.involuntary_switches += 1;
                let target = select_cpu_after_preemption(thread, scheduling.cpu_affinity);
                Some((
                    target,
                    commit_ready_transition(scheduling.transition_to_ready(target)),
                ))
            }
            _ => None,
        }
    };
    if let Some((target, generation)) = ready {
        deliver_ready_entry(target, ready_entry(thread.clone(), generation));
    }
}
