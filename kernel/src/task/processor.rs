use crate::arch::context::KernelContext;
use crate::sync::WaitResult;
use crate::sync::{IrqMutex, LocalIrqGuard, LocalIrqTransfer};
use crate::{
    cpu::{self, CpuId, CpuSet},
    platform,
    task::{
        CpuAffinity, ReadyRetirement, ReadyTransition, RunState, StopResume, StopTransition,
        TaskControlBlock, WaitMembership,
        scheduler::cfs_scheduler::{CfsRunQueue, RunQueueEntry},
    },
};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

mod handoff;
mod job_control;
mod placement;
mod ready_membership;
mod ready_queue;
pub(in crate::task) use handoff::{
    publish_pending_handoff, resume_without_switch, take_pending_handoff,
};
pub(in crate::task) use job_control::request_tick_reschedule;
pub(super) use job_control::{
    begin_preempt_running_task, continue_stopped_task, request_task_reschedule, request_task_stop,
};
pub(crate) use placement::enqueue_new_task;
use placement::{ready_entry, select_cpu};
use ready_membership::{commit_ready_retirement, commit_ready_transition};

/// context switch 异常返回时的 fail-stop 目标。
///
/// # Returns
///
/// 永不返回。
pub(crate) fn idle_return() -> ! {
    panic!("idle context returned unexpectedly");
}

/// 仅由所属 CPU 可变访问的调度执行状态。
pub(crate) struct Processor {
    cpu_id: CpuId,
    pub(crate) current: Option<Arc<TaskControlBlock>>,
    idle_context: KernelContext,
    runqueue: CfsRunQueue,
    pending_handoff: Option<handoff::PendingHandoff>,
    deferred_reap: Option<Arc<TaskControlBlock>>,
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

    /// 获取当前 CPU idle context 的稳定地址。
    ///
    /// # Returns
    ///
    /// 指向当前 CPU `KernelContext` 的唯一可变指针。
    pub(crate) fn idle_context_ptr(&mut self) -> *mut KernelContext {
        &mut self.idle_context
    }

    /// 把已完成 Ready 状态转换的 entry 加入本地 runqueue。
    ///
    /// # Parameters
    ///
    /// - `entry`: generation 必须对应 `Ready { cpu: self }`。
    ///
    /// # Returns
    ///
    /// Ready entity 的 vruntime 严格早于 current 时返回 true，供 delivery 决定 reschedule。
    pub(crate) fn add_ready_entry(&mut self, entry: RunQueueEntry) -> bool {
        ready_queue::add_ready_entry(self, entry)
    }

    /// 消费 stale entry，原子完成 Ready → Running 与 current 发布。
    ///
    /// # Returns
    ///
    /// 队列为空时返回 `None`，否则返回唯一取出的任务引用。
    pub(crate) fn select_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        ready_queue::select_task(self)
    }

    /// 撤销当前 CPU 的 running ownership 与负载发布。
    ///
    /// # Returns
    ///
    /// 当前 Task；空 current 表示调用路径破坏调度状态并返回 None。
    pub(crate) fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        let current = self.current.take()?;
        let previous = current_per_cpu()
            .running_entries
            .fetch_sub(1, Ordering::Relaxed);
        assert_eq!(previous, 1, "running load counter lost current ownership");
        Some(current)
    }

    /// 把远端 mailbox 中的任务转移到当前 CPU scheduler。
    pub(crate) fn drain_inbound_to_local(&mut self) {
        ready_queue::drain_inbound_to_local(self);
    }

    fn defer_reap(&mut self, task: Arc<TaskControlBlock>) {
        assert!(
            self.deferred_reap.is_none(),
            "deferred reap slot must be drained before another task exits"
        );
        self.deferred_reap = Some(task);
    }

    fn take_deferred_reap(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.deferred_reap.take()
    }
}

struct PerCpuProcessor {
    local: UnsafeCell<MaybeUninit<Processor>>,
    initialized: AtomicBool,
    // OWNER: owner CPU 在 IRQ-disabled borrow 内独占 `local`；嵌套 `with_current_processor`
    // 会同时存在两个 `&mut Processor`，属于 undefined behavior。缺失该 flag 时只能靠文档约束闭包。
    borrowed: AtomicBool,
    // SchedulingState transition 同锁发布该 CPU 的精确 Ready membership 数；它只投影
    // logical load，不拥有 heap/mailbox token，Relaxed 过期值只影响瞬时选核 hint。
    ready_entries: AtomicUsize,
    // OWNER: processor slot 发布当前 CPU 的 Running membership；缺失会让选核把 busy CPU 当成 idle。
    running_entries: AtomicUsize,
    // OWNER: per-CPU reschedule request 可由远端 stop signal 发布，当前 CPU trap return 唯一消费。
    // 若仍保存在 local Processor，远端 IPI 只能唤醒而不能阻止目标 Thread 返回用户态。
    reschedule_requested: AtomicBool,
    // OWNER: owner CPU 发布 local Ready 队列的 vruntime floor；remote creator 只读取并与
    // inbound snapshot 合并。缺失时 fork churn 可持续插队，饿死已经 runnable 的 task。
    // 过期值只改变新 task 排序，不拥有或发布 scheduler membership。
    placement_vruntime: AtomicU64,
    // OWNER: processor slot 累计当前 CPU 已提交的 task runtime；缺失会使 /proc/stat 无法区分 busy/idle。
    busy_us: AtomicU64,
    // timer softirq 可远端投递 runnable task；IRQ-safe lock 防止打断当前 CPU drain 后再入。
    inbound: IrqMutex<VecDeque<RunQueueEntry>>,
    queue_capacity: usize,
}

impl PerCpuProcessor {
    /// 创建尚未由 owner CPU 初始化的 processor slot。
    ///
    /// # Returns
    ///
    /// 空 local processor、mailbox 和负载计数。
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
            inbound: IrqMutex::new(inbound),
            queue_capacity,
        }
    }
}

pub(super) fn account_current_cpu_runtime(runtime_us: u64) {
    current_per_cpu()
        .busy_us
        .fetch_add(runtime_us, Ordering::Relaxed);
}

pub(crate) fn cpu_runtime_snapshot() -> Result<Vec<(usize, u64)>, ()> {
    let slots = &PROCESSOR_TOPOLOGY.wait().slots;
    let mut snapshot = Vec::new();
    snapshot.try_reserve_exact(slots.len()).map_err(|_| ())?;
    snapshot.extend(slots.iter().map(|slot| {
        (
            slot.cpu_id.index(),
            slot.processor.busy_us.load(Ordering::Relaxed),
        )
    }));
    Ok(snapshot)
}

// SAFETY: `local` 只能由 ID 等于所属 ProcessorSlot 的执行流访问；远端 CPU 只能触及
// Ready/Running 投影和 inbound Mutex。trap 入口保持 local interrupt 关闭，因此同 CPU 不会重入 local 可变借用。
unsafe impl Sync for PerCpuProcessor {}

struct ProcessorSlot {
    cpu_id: CpuId,
    processor: PerCpuProcessor,
}

struct ProcessorTopology {
    slots: Box<[ProcessorSlot]>,
}

// OWNER: processor module owns scheduler-local state for every platform CPU.
static PROCESSOR_TOPOLOGY: spin::Once<ProcessorTopology> = spin::Once::new();

/// 按 CpuTopology 的 logical-index 顺序构造唯一 scheduler processor slots。
///
/// # Errors
///
/// 重复初始化或 arch/task topology 顺序分裂时 fail-stop。
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
        "physical memory cannot host one task stack"
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

// OWNER: processor module owns the round-robin cursor used for initial task placement.
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);

#[inline(always)]
fn processor_at(index: usize) -> &'static PerCpuProcessor {
    &PROCESSOR_TOPOLOGY.wait().slots[index].processor
}

#[inline(always)]
fn current_slot() -> &'static ProcessorSlot {
    &PROCESSOR_TOPOLOGY.wait().slots[cpu::current_id().index()]
}

#[inline(always)]
fn current_per_cpu() -> &'static PerCpuProcessor {
    processor_at(cpu::current_id().index())
}

/// 在关闭本地 S-mode 中断期间访问当前 CPU 独占的 processor。
///
/// # Parameters
///
/// - `f`: 不得保存或泄漏 `Processor` 引用的同步闭包。
///
/// # Returns
///
/// 闭包的返回值。
///
/// # Errors
///
/// `tp` 越界属于内核不变量破坏。
pub(crate) fn with_current_processor<R>(f: impl FnOnce(&mut Processor) -> R) -> R {
    let _irq = LocalIrqGuard::disable();
    // 1. 中断关闭保证同 CPU 的 trap handler 不能在该 mutable borrow 存活时再次借用；
    // 2. borrowed flag 拒绝闭包内的嵌套调用，使唯一 `&mut Processor` 由运行时而非文档保证；
    // 3. kernel panic 不展开，因此只在闭包正常返回后释放 flag。
    let slot = current_slot();
    let processor = &slot.processor;
    assert!(
        !processor.borrowed.swap(true, Ordering::Relaxed),
        "nested current Processor borrow"
    );
    // initialized 只由当前 CPU 在关闭 local interrupt 时读写，不承担跨 CPU 发布；缺失会重复构造 Processor。
    if !processor.initialized.load(Ordering::Relaxed) {
        // SAFETY: 只有当前 logical CPU 能到达自己的 slot.local，且 borrowed flag 证明无其他引用。
        unsafe {
            (*processor.local.get()).write(Processor::new(slot.cpu_id, processor.queue_capacity));
        }
        processor.initialized.store(true, Ordering::Relaxed);
    }
    // SAFETY: initialized 证明对象已构造；IRQ guard 与 borrowed flag 证明这是该 CPU 上唯一
    // 存活的 `&mut Processor`，且引用不超出本次闭包调用。
    let local = unsafe { (*processor.local.get()).assume_init_mut() };
    let result = f(local);
    processor.borrowed.store(false, Ordering::Relaxed);
    result
}

/// 将当前 exiting Task 在 task stack 上的 owner 移交给所属 CPU。
///
/// # Parameters
///
/// - `task`: 必须是已从 current、PID index 与 runqueue 移除的退出任务。
///
/// # Returns
///
/// 无返回值；slot 未先 drain 表示 terminal ownership 协议损坏并 panic。
pub(super) fn defer_task_reap(task: Arc<TaskControlBlock>) {
    with_current_processor(|processor| processor.defer_reap(task));
}

/// 在 idle stack 上取得并释放 deferred exiting Task。
///
/// # Returns
///
/// slot 为空时不执行操作；存在任务时 deferred Arc 在本函数返回前于 idle stack Drop。
pub(super) fn reap_deferred_task() {
    let task = with_current_processor(Processor::take_deferred_reap);
    drop(task);
}

/// 标记当前 CPU 在返回用户态前需要重新调度。
///
/// # Returns
///
/// 无返回值；flag 仅由当前 CPU 在关中断临界区访问。
pub(crate) fn request_reschedule() {
    current_per_cpu()
        .reschedule_requested
        .store(true, Ordering::Release);
}

/// 消费当前 CPU 的 reschedule flag。
///
/// # Returns
///
/// 本次用户态返回是否应先 yield。
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

/// 投递 Ready entry；busy target 同步 reschedule，避免 syscall writer 饿死 Ready reader。
///
/// # Parameters
///
/// - `cpu_id`: 目标 CPU ID。
/// - `entry`: 带 generation 的 membership token。
///
/// # Errors
///
/// 目标越界、未 active 或 platform IPI 失败均触发内核不变量失败，不做 CPU fallback。
fn deliver_ready_entry(cpu_id: CpuId, entry: RunQueueEntry) {
    ready_queue::deliver_ready_entry(cpu_id, entry);
}

/// 原子替换 Thread affinity，并迁移位于已禁止 CPU 的 Ready membership。
///
/// # Parameters
///
/// - `task`: ProcessTable process graph 定位并保活的 live Thread。
/// - `affinity`: 已与 active topology 相交且非空的新 affinity。
///
/// # Returns
///
/// 无返回值；Ready entry 已迁移，Running migration 由 affinity orchestration 同步完成。
///
/// # Errors
///
/// 无可恢复错误；无 active CPU 或状态不变量破坏时 fail-stop。
pub(in crate::task) fn replace_task_affinity(task: &Arc<TaskControlBlock>, affinity: CpuAffinity) {
    let mut replacement = None;
    let mut stale_cpu = None;
    {
        let mut scheduling = task.scheduling.state.lock();
        scheduling.cpu_affinity = affinity;
        if let RunState::Ready { cpu, .. } = scheduling.run_state()
            && !affinity.allows(cpu)
        {
            let target = select_cpu(task, affinity);
            let generation = commit_ready_transition(scheduling.transition_to_ready(target));
            replacement = Some((target, generation));
            stale_cpu = Some(cpu);
        }
    }
    if let Some((cpu, generation)) = replacement {
        deliver_ready_entry(cpu, ready_entry(task.clone(), generation));
    }
    if let Some(cpu) = stale_cpu {
        job_control::request_reschedule_on(cpu);
    }
}

/// 消费一个明确 deadline wait membership，并完成无丢失唤醒转换。
///
/// # Parameters
///
/// - `task`: wait queue 移出的 task owner。
/// - `wait_id`: 必须与 SchedulingState 中记录的 ID 相同。
/// - `result`: deadline 到期或 signal interruption 的唯一结果。
///
/// # Returns
///
/// 本次调用真正消费 membership 时返回 true；重复/stale wake 返回 false。
pub(super) fn wake_deadline_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Deadline(wait_id), Some(result))
}

/// 消费 child-exit wait membership，并完成无丢失唤醒转换。
///
/// # Parameters
///
/// - `task`: Process graph 移出的唯一 waiter owner。
/// - `result`: child exit 或 signal interruption 的唯一结果。
///
/// # Returns
///
/// membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_child_task(task: Arc<TaskControlBlock>, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Child, Some(result))
}

/// 消费 futex wait membership，并发布 wake/timeout/interruption 结果。
///
/// # Parameters
///
/// - `task`: indexed wait registry 移出的 task owner。
/// - `wait_id`: 必须与 SchedulingState 中记录的 ID 相同。
/// - `result`: futex wait 的唯一完成结果。
///
/// # Returns
///
/// membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_futex_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Futex(wait_id), Some(result))
}

/// 消费 console wait membership，并完成 deferred IRQ wake 转换。
///
/// # Parameters
///
/// - `task`: indexed wait registry 移出的 task owner。
/// - `wait_id`: 必须与 SchedulingState 中记录的 ID 相同。
/// - `result`: UART input 或 VTIME deadline 的唯一完成结果。
///
/// # Returns
///
/// membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_console_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Console(wait_id), Some(result))
}

/// 消费 `rt_sigtimedwait` membership，并发布 signal/timeout/interruption 结果。
///
/// # Parameters
///
/// - `task`: indexed wait registry 移出的 task owner。
/// - `wait_id`: 必须与 claimed registration 和 SchedulingState 同时一致。
/// - `result`: 匹配 signal、timeout 或无关 signal interruption。
///
/// # Returns
///
/// membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_signal_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Signal(wait_id), Some(result))
}

pub(super) fn wake_pipe_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Pipe(wait_id), Some(result))
}

pub(super) fn wake_flock_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::AdvisoryLock(wait_id), Some(result))
}

pub(super) fn wake_poll_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Poll(wait_id), Some(result))
}

/// 消费指定 wait membership，并经 scheduler 唯一状态机发布 ready transition。
///
/// # Parameters
///
/// - `task`: wait owner 移出的 blocked task Arc。
/// - `expected`: 调用方持有的精确 wait identity。
/// - `result`: 恢复后由 blocked syscall 消费的完成结果。
///
/// # Returns
///
/// membership 匹配并成功消费返回 true；stale wake 返回 false。
///
/// # Errors
///
/// 无错误；状态不变量破坏时 fail-stop。
pub(in crate::task) fn wake_waiting_task(
    task: Arc<TaskControlBlock>,
    expected: WaitMembership,
    result: Option<WaitResult>,
) -> bool {
    let ready = {
        let mut scheduling = task.scheduling.state.lock();
        if scheduling.wait != Some(expected) {
            return false;
        }
        scheduling.wait = None;
        assert!(scheduling.wait_result.is_none());
        scheduling.wait_result = result;
        match scheduling.run_state() {
            RunState::Blocking { cpu } => {
                scheduling.replace_non_ready_state(RunState::WakePending { cpu });
                None
            }
            RunState::Blocked => {
                let target_cpu = select_cpu(&task, scheduling.cpu_affinity);
                let generation =
                    commit_ready_transition(scheduling.transition_to_ready(target_cpu));
                Some((target_cpu, generation))
            }
            RunState::Stopped {
                resume: StopResume::Blocked,
            } => {
                scheduling.replace_non_ready_state(RunState::Stopped {
                    resume: StopResume::Runnable,
                });
                None
            }
            RunState::StopPending {
                cpu,
                transition: StopTransition::Blocking,
            } => {
                scheduling.replace_non_ready_state(RunState::StopPending {
                    cpu,
                    transition: StopTransition::WakePending,
                });
                None
            }
            RunState::Exited => None,
            state => panic!("wait membership attached to invalid state {state:?}"),
        }
    };
    if let Some((cpu, generation)) = ready {
        deliver_ready_entry(cpu, ready_entry(task, generation));
    }
    true
}

/// 在 next task 或 idle continuation 上完成 Blocking/WakePending/Preempting handoff。
///
/// # Parameters
///
/// - `task`: context 已由该 CPU 保存、pending slot 唯一保活的 outgoing task。
///
/// # Returns
///
/// 无返回值；Ready 只在 task context 已停止执行后发布。
pub(super) fn finish_deschedule_transition(task: &Arc<TaskControlBlock>) -> bool {
    let cpu = cpu::current_id();
    let mut stopped = false;
    let ready = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state() {
            RunState::Blocking { cpu: owner } => {
                assert_eq!(owner, cpu, "blocking task returned on another CPU");
                scheduling.replace_non_ready_state(RunState::Blocked);
                None
            }
            RunState::WakePending { cpu: owner } => {
                assert_eq!(owner, cpu, "wake-pending task returned on another CPU");
                let target = if scheduling.cpu_affinity.allows(cpu) {
                    cpu
                } else {
                    select_cpu(task, scheduling.cpu_affinity)
                };
                let generation = commit_ready_transition(scheduling.transition_to_ready(target));
                Some((target, generation))
            }
            RunState::Preempting { cpu: owner } => {
                assert_eq!(owner, cpu, "preempting task returned on another CPU");
                let target_cpu = select_cpu(task, scheduling.cpu_affinity);
                let generation =
                    commit_ready_transition(scheduling.transition_to_ready(target_cpu));
                Some((target_cpu, generation))
            }
            RunState::StopPending {
                cpu: owner,
                transition,
            } => {
                assert_eq!(owner, cpu, "stopping task returned on another CPU");
                scheduling.replace_non_ready_state(RunState::Stopped {
                    resume: match transition {
                        StopTransition::Blocking => StopResume::Blocked,
                        StopTransition::Running
                        | StopTransition::Preempting
                        | StopTransition::WakePending => StopResume::Runnable,
                    },
                });
                stopped = true;
                None
            }
            _ => None,
        }
    };
    if let Some((target_cpu, generation)) = ready {
        deliver_ready_entry(target_cpu, ready_entry(task.clone(), generation));
    }
    stopped
}
