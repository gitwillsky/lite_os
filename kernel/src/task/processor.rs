use crate::sync::{IrqMutex, LocalIrqGuard};
use crate::{
    arch::{
        hart::{self, hart_id},
        sbi,
    },
    task::{
        CpuAffinity, RunState, StopResume, StopTransition, TaskControlBlock, WaitMembership,
        WaitResult,
        context::TaskContext,
        scheduler::cfs_scheduler::{CfsRunQueue, RunQueueEntry},
    },
};
use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

mod job_control;
mod placement;
pub(in crate::task) use job_control::request_tick_reschedule;
pub(super) use job_control::{
    begin_preempt_running_task, continue_stopped_task, request_task_reschedule, request_task_stop,
};
pub(crate) use placement::enqueue_new_task;
use placement::{ready_entry, select_cpu};

/// @description context switch 异常返回时的 fail-stop 目标。
///
/// @return 永不返回。
#[unsafe(no_mangle)]
pub(crate) extern "C" fn idle_return() -> ! {
    panic!("idle context returned unexpectedly");
}

/// @description 仅由所属 hart 可变访问的调度执行状态。
pub(crate) struct Processor {
    hart_id: usize,
    pub(crate) current: Option<Arc<TaskControlBlock>>,
    idle_context: TaskContext,
    runqueue: CfsRunQueue,
    deferred_reap: Option<Arc<TaskControlBlock>>,
}

impl Processor {
    fn new(hart_id: usize, queue_capacity: usize) -> Self {
        let mut idle_context = TaskContext::zero_init();
        idle_context.set_ra(idle_return as *const () as usize);
        Self {
            hart_id,
            current: None,
            idle_context,
            runqueue: CfsRunQueue::try_with_capacity(queue_capacity)
                .expect("scheduler runqueue allocation failed"),
            deferred_reap: None,
        }
    }

    /// @description 获取当前 hart idle context 的稳定地址。
    ///
    /// @return 指向本 hart `TaskContext` 的唯一可变指针。
    pub(crate) fn idle_context_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_context
    }

    /// @description 把已完成 Ready 状态转换的 entry 加入本地 runqueue。
    ///
    /// @param entry generation 必须对应 `Ready { cpu: self }`。
    /// @return 无返回值。
    pub(crate) fn add_ready_entry(&mut self, entry: RunQueueEntry) {
        let slot = current_per_hart();
        let stale = self
            .runqueue
            .retain(|candidate| candidate.is_current_ready(self.hart_id));
        if stale != 0 {
            slot.queued_entries.fetch_sub(stale, Ordering::Relaxed);
        }
        self.runqueue.push(entry);
        let floor = self
            .runqueue
            .minimum_vruntime()
            .expect("non-empty runqueue lost placement floor");
        slot.placement_vruntime.store(floor, Ordering::Release);
        // queued_entries 与 local heap 每次 push/pop 一一对应，不统计 mailbox/current。
        slot.queued_entries.fetch_add(1, Ordering::Relaxed);
        debug_assert_eq!(
            self.runqueue.len(),
            slot.queued_entries.load(Ordering::Relaxed)
        );
    }

    /// @description 消费 stale entry，原子完成 Ready → Running 与 current 发布。
    ///
    /// @return 队列为空时返回 `None`，否则返回唯一取出的任务引用。
    pub(crate) fn select_task(&mut self) -> Option<Arc<TaskControlBlock>> {
        assert!(self.current.is_none(), "CPU already owns a current task");
        let slot = current_per_hart();
        loop {
            let entry = self.runqueue.pop()?;
            slot.queued_entries.fetch_sub(1, Ordering::Relaxed);
            let mut scheduling = entry.task.scheduling.state.lock();
            match scheduling.run_state {
                RunState::Ready { cpu, generation }
                    if cpu == self.hart_id && generation == entry.generation =>
                {
                    scheduling.run_state = RunState::Running { cpu: self.hart_id };
                    drop(scheduling);
                    self.current = Some(entry.task.clone());
                    let floor = self.runqueue.minimum_vruntime().unwrap_or(entry.vruntime);
                    slot.placement_vruntime.store(floor, Ordering::Release);
                    slot.running_entries.fetch_add(1, Ordering::Relaxed);
                    return Some(entry.task);
                }
                _ => {
                    // generation 不匹配说明 stop/continue 等转换已废弃该 entry，只消费不执行。
                }
            }
        }
    }

    /// @description 撤销当前 hart 的 running ownership 与负载发布。
    ///
    /// @return 当前 Task；空 current 表示调用路径破坏调度状态并返回 None。
    pub(crate) fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        let current = self.current.take()?;
        let previous = current_per_hart()
            .running_entries
            .fetch_sub(1, Ordering::Relaxed);
        assert_eq!(previous, 1, "running load counter lost current ownership");
        Some(current)
    }

    /// @description 把远端 mailbox 中的任务转移到本 hart scheduler。
    ///
    /// @return 无返回值。
    pub(crate) fn drain_inbound_to_local(&mut self) {
        let slot = current_per_hart();
        // 只消费进入本轮时的 snapshot 数量，保留 VecDeque 的预留 backing
        // storage；如果 swap 给空 VecDeque，下一次 IRQ wake 会重新分配。
        let count = slot.inbound.lock().len();
        for _ in 0..count {
            let entry = slot
                .inbound
                .lock()
                .pop_front()
                .expect("inbound snapshot shrank without owner");
            slot.inbound_entries.fetch_sub(1, Ordering::Relaxed);
            self.add_ready_entry(entry);
        }
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

struct PerHartProcessor {
    local: UnsafeCell<MaybeUninit<Processor>>,
    initialized: AtomicBool,
    // 仅供跨 hart 负载选择；Relaxed 过期值只影响选择，不发布或拥有 runqueue entry。
    queued_entries: AtomicUsize,
    // 与 inbound mutex 内容器同步增减；Relaxed 读取只作为近似负载 hint。
    inbound_entries: AtomicUsize,
    // OWNER: processor slot 发布本 hart 当前 Running membership；缺失会让选核把 busy hart 当成 idle。
    running_entries: AtomicUsize,
    // OWNER: per-hart reschedule request 可由远端 stop signal 发布，本 hart trap return 唯一消费。
    // 若仍保存在 local Processor，远端 IPI 只能唤醒而不能阻止目标 Thread 返回用户态。
    reschedule_requested: AtomicBool,
    // OWNER: owner hart 发布 local Ready 队列的 vruntime floor；remote creator 只读取并与
    // inbound snapshot 合并。缺失时 fork churn 可持续插队，饿死已经 runnable 的 task。
    // 过期值只改变新 task 排序，不拥有或发布 scheduler membership。
    placement_vruntime: AtomicU64,
    // OWNER: processor slot 累计本 hart 已提交的 task runtime；缺失会使 /proc/stat 无法区分 busy/idle。
    busy_us: AtomicU64,
    // timer softirq 可远端投递 runnable task；IRQ-safe lock 防止打断本 hart drain 后再入。
    inbound: IrqMutex<VecDeque<RunQueueEntry>>,
    queue_capacity: usize,
}

impl PerHartProcessor {
    /// @description 创建尚未由 owner hart 初始化的 processor slot。
    ///
    /// @return 空 local processor、mailbox 和负载计数。
    /// @errors 无错误。
    fn new(queue_capacity: usize) -> Self {
        let mut inbound = VecDeque::new();
        inbound
            .try_reserve_exact(queue_capacity)
            .expect("scheduler inbound allocation failed");
        Self {
            local: UnsafeCell::new(MaybeUninit::uninit()),
            initialized: AtomicBool::new(false),
            queued_entries: AtomicUsize::new(0),
            inbound_entries: AtomicUsize::new(0),
            running_entries: AtomicUsize::new(0),
            reschedule_requested: AtomicBool::new(false),
            placement_vruntime: AtomicU64::new(0),
            busy_us: AtomicU64::new(0),
            inbound: IrqMutex::new(inbound),
            queue_capacity,
        }
    }
}

pub(super) fn account_current_hart_runtime(runtime_us: u64) {
    current_per_hart()
        .busy_us
        .fetch_add(runtime_us, Ordering::Relaxed);
}

pub(crate) fn cpu_runtime_snapshot() -> Result<Vec<(usize, u64)>, ()> {
    let slots = &PROCESSOR_TOPOLOGY.wait().slots;
    let mut snapshot = Vec::new();
    snapshot.try_reserve_exact(slots.len()).map_err(|_| ())?;
    snapshot.extend(
        slots
            .iter()
            .map(|slot| (slot.hart_id, slot.processor.busy_us.load(Ordering::Relaxed))),
    );
    Ok(snapshot)
}

// SAFETY: `local` 只能由 ID 等于所属 ProcessorSlot 的执行流访问；远端 hart 只能触及
// queued/inbound 计数和 inbound Mutex。trap 入口保持 SIE 关闭，因此同 hart 不会重入 local 可变借用。
unsafe impl Sync for PerHartProcessor {}

struct ProcessorSlot {
    hart_id: usize,
    processor: PerHartProcessor,
}

struct ProcessorTopology {
    slots: Box<[ProcessorSlot]>,
}

// OWNER: processor module owns scheduler-local state for every DTB hart.
static PROCESSOR_TOPOLOGY: spin::Once<ProcessorTopology> = spin::Once::new();

/// @description 按 HartTopology 的 compact-index 顺序构造唯一 scheduler processor slots。
///
/// @return 无返回值。
/// @errors 重复初始化或 arch/task topology 顺序分裂时 fail-stop。
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
        .try_reserve_exact(hart::hart_count())
        .expect("processor topology allocation failed");
    for state in hart::states() {
        let index = slots.len();
        assert_eq!(
            hart::hart_index(state.hart_id()),
            Some(index),
            "processor topology order diverged from compact hart index"
        );
        slots.push(ProcessorSlot {
            hart_id: state.hart_id(),
            processor: PerHartProcessor::new(queue_capacity),
        });
    }
    PROCESSOR_TOPOLOGY.call_once(|| ProcessorTopology {
        slots: slots.into_boxed_slice(),
    });
}

// OWNER: processor module owns the round-robin cursor used for initial task placement.
static NEXT_CPU: AtomicUsize = AtomicUsize::new(0);

#[inline(always)]
fn processor_at(index: usize) -> &'static PerHartProcessor {
    &PROCESSOR_TOPOLOGY.wait().slots[index].processor
}

#[inline(always)]
fn current_slot() -> &'static ProcessorSlot {
    &PROCESSOR_TOPOLOGY.wait().slots[hart::current_hart_index()]
}

#[inline(always)]
fn current_per_hart() -> &'static PerHartProcessor {
    processor_at(hart::current_hart_index())
}

fn local_processor() -> &'static mut Processor {
    let slot = current_slot();
    let hart = slot.hart_id;
    let processor = &slot.processor;
    // initialized 只由本 hart 在关闭 SIE 时读写，不承担跨 hart 发布；缺失该分支会重复构造 Processor。
    if !processor.initialized.load(Ordering::Relaxed) {
        // SAFETY: 只有 hart `hart` 能到达自己的 slot.local，且 S-mode trap 不开启嵌套中断。
        unsafe {
            (*processor.local.get()).write(Processor::new(hart, processor.queue_capacity));
        }
        processor.initialized.store(true, Ordering::Relaxed);
    }
    // SAFETY: 与上面的 per-hart 唯一所有权约束相同，initialized 证明对象已构造。
    unsafe { (*processor.local.get()).assume_init_mut() }
}

/// @description 在关闭本地 S-mode 中断期间访问当前 hart 独占的 processor。
///
/// @param f 不得保存或泄漏 `Processor` 引用的同步闭包。
/// @return 闭包的返回值。
/// @errors `tp` 越界属于内核不变量破坏。
pub(crate) fn with_current_processor<R>(f: impl FnOnce(&mut Processor) -> R) -> R {
    let _irq = LocalIrqGuard::disable();
    // 中断关闭保证同 hart 的 trap handler 不能在该 mutable borrow 存活时再次借用 local processor。
    f(local_processor())
}

/// @description 将当前 exiting Task 在 task stack 上的 owner 移交给所属 hart。
///
/// @param task 必须是已从 current、PID index 与 runqueue 移除的退出任务。
/// @return 无返回值；slot 未先 drain 表示 terminal ownership 协议损坏并 panic。
pub(super) fn defer_task_reap(task: Arc<TaskControlBlock>) {
    with_current_processor(|processor| processor.defer_reap(task));
}

/// @description 在 idle stack 上取得并释放 deferred exiting Task。
///
/// @return slot 为空时不执行操作；存在任务时 deferred Arc 在本函数返回前于 idle stack Drop。
pub(super) fn reap_deferred_task() {
    let task = with_current_processor(Processor::take_deferred_reap);
    drop(task);
}

/// @description 标记当前 hart 在返回用户态前需要重新调度。
///
/// @return 无返回值；flag 仅由本 hart 在关中断临界区访问。
pub(crate) fn request_reschedule() {
    current_per_hart()
        .reschedule_requested
        .store(true, Ordering::Release);
}

/// @description 消费当前 hart 的 reschedule flag。
///
/// @return 本次用户态返回是否应先 yield。
pub(crate) fn take_reschedule() -> bool {
    current_per_hart()
        .reschedule_requested
        .swap(false, Ordering::AcqRel)
}

fn publish_reschedule_at(cpu_index: usize) {
    let target = &PROCESSOR_TOPOLOGY.wait().slots[cpu_index];
    target
        .processor
        .reschedule_requested
        .store(true, Ordering::Release);
    if target.hart_id != hart_id() {
        sbi::sbi_send_ipi(1usize << target.hart_id, 0)
            .expect("SBI IPI failed for remote reschedule");
    }
}

/// @description 投递 Ready entry；busy target 同步 reschedule，避免 syscall writer 饿死 Ready reader。
///
/// @param cpu_id 目标 hart ID。
/// @param entry 带 generation 的 membership token。
/// @return 无返回值。
/// @errors 目标越界、未 active 或 SBI IPI 失败均触发内核不变量失败，不做 CPU fallback。
fn deliver_ready_entry(cpu_id: usize, entry: RunQueueEntry) {
    let current = hart_id();
    if cpu_id == current {
        with_current_processor(|processor| processor.add_ready_entry(entry));
        if current_per_hart().running_entries.load(Ordering::Relaxed) != 0 {
            request_reschedule();
        }
        return;
    }

    let target_index = hart::hart_index(cpu_id)
        .unwrap_or_else(|| panic!("target CPU {} is absent from DTB topology", cpu_id));
    let target_state = &hart::states()[target_index];
    let target = processor_at(target_index);
    assert!(target_state.is_active());
    let mut inbound = target.inbound.lock();
    let before = inbound.len();
    inbound.retain(|candidate| candidate.is_current_ready(cpu_id));
    target
        .inbound_entries
        .fetch_sub(before - inbound.len(), Ordering::Relaxed);
    assert!(
        inbound.len() < target.queue_capacity,
        "preallocated scheduler mailbox capacity exhausted"
    );
    inbound.push_back(entry);
    target.inbound_entries.fetch_add(1, Ordering::Relaxed);
    drop(inbound);
    publish_reschedule_at(target_index);
}

impl RunQueueEntry {
    /// @description 核对 entry generation 与唯一 SchedulingState Ready membership。
    /// @param cpu 容器所属 hart ID。
    /// @return 该 entry 仍是当前唯一 Ready membership 时返回 true。
    fn is_current_ready(&self, cpu: usize) -> bool {
        matches!(
            self.task.scheduling.state.lock().run_state,
            RunState::Ready {
                cpu: owner,
                generation
            } if owner == cpu && generation == self.generation
        )
    }
}

/// @description 原子替换 Thread affinity，并迁移位于已禁止 CPU 的 Ready membership。
///
/// @param task TaskManager process graph 定位并保活的 live Thread。
/// @param affinity 已与 active topology 相交且非空的新 affinity。
/// @return 无返回值；Ready entry 已迁移，Running migration 由 affinity orchestration 同步完成。
/// @errors 无可恢复错误；无 active CPU 或状态不变量破坏时 fail-stop。
pub(in crate::task) fn replace_task_affinity(task: &Arc<TaskControlBlock>, affinity: CpuAffinity) {
    let mut replacement = None;
    let mut stale_cpu = None;
    {
        let mut scheduling = task.scheduling.state.lock();
        scheduling.cpu_affinity = affinity;
        if let RunState::Ready { cpu, .. } = scheduling.run_state
            && !affinity.allows_hart(cpu)
        {
            let target = select_cpu(task, affinity);
            let generation = scheduling.transition_to_ready(target);
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

/// @description 消费一个明确 deadline wait membership，并完成无丢失唤醒转换。
///
/// @param task wait queue 移出的 task owner。
/// @param wait_id 必须与 SchedulingState 中记录的 ID 相同。
/// @param result deadline 到期或 signal interruption 的唯一结果。
/// @return 本次调用真正消费 membership 时返回 true；重复/stale wake 返回 false。
pub(super) fn wake_deadline_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Deadline(wait_id), Some(result))
}

/// @description 消费 child-exit wait membership，并完成无丢失唤醒转换。
///
/// @param task Process graph 移出的唯一 waiter owner。
/// @param result child exit 或 signal interruption 的唯一结果。
/// @return membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_child_task(task: Arc<TaskControlBlock>, result: WaitResult) -> bool {
    wake_waiting_task(task, WaitMembership::Child, Some(result))
}

/// @description 消费 futex wait membership，并发布 wake/timeout/interruption 结果。
///
/// @param task indexed wait registry 移出的 task owner。
/// @param wait_id 必须与 SchedulingState 中记录的 ID 相同。
/// @param result futex wait 的唯一完成结果。
/// @return membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_futex_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Futex(wait_id), Some(result))
}

/// @description 消费 console wait membership，并完成 deferred IRQ wake 转换。
///
/// @param task indexed wait registry 移出的 task owner。
/// @param wait_id 必须与 SchedulingState 中记录的 ID 相同。
/// @param result UART input 或 VTIME deadline 的唯一完成结果。
/// @return membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_console_task(
    task: Arc<TaskControlBlock>,
    wait_id: u64,
    result: WaitResult,
) -> bool {
    wake_waiting_task(task, WaitMembership::Console(wait_id), Some(result))
}

/// @description 消费 `rt_sigtimedwait` membership，并发布 signal/timeout/interruption 结果。
///
/// @param task indexed wait registry 移出的 task owner。
/// @param result 匹配 signal、timeout 或无关 signal interruption。
/// @return membership 有效时返回 true；stale wake 返回 false。
pub(super) fn wake_signal_task(task: Arc<TaskControlBlock>, result: WaitResult) -> bool {
    let wait_id = match task.scheduling.state.lock().wait {
        Some(WaitMembership::Signal(id)) => id,
        _ => return false,
    };
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

/// @description 消费指定 wait membership，并经 scheduler 唯一状态机发布 ready transition。
/// @param task wait owner 移出的 blocked task Arc。
/// @param expected 调用方持有的精确 wait identity。
/// @param result 恢复后由 blocked syscall 消费的完成结果。
/// @return membership 匹配并成功消费返回 true；stale wake 返回 false。
/// @errors 无错误；状态不变量破坏时 fail-stop。
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
        match scheduling.run_state {
            RunState::Blocking { cpu } => {
                scheduling.run_state = RunState::WakePending { cpu };
                None
            }
            RunState::Blocked => {
                let target_cpu = select_cpu(&task, scheduling.cpu_affinity);
                let generation = scheduling.transition_to_ready(target_cpu);
                Some((target_cpu, generation))
            }
            RunState::Stopped {
                resume: StopResume::Blocked,
            } => {
                scheduling.run_state = RunState::Stopped {
                    resume: StopResume::Runnable,
                };
                None
            }
            RunState::StopPending {
                cpu,
                transition: StopTransition::Blocking,
            } => {
                scheduling.run_state = RunState::StopPending {
                    cpu,
                    transition: StopTransition::WakePending,
                };
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

/// @description 在 idle stack 上完成 Blocking/WakePending/Preempting 的切出握手。
///
/// @param task 刚从该 CPU 切回 idle 的 task。
/// @return 无返回值；Ready 只在 task context 已停止执行后发布。
pub(super) fn finish_deschedule_transition(task: &Arc<TaskControlBlock>) -> bool {
    let cpu = hart_id();
    let mut stopped = false;
    let ready = {
        let mut scheduling = task.scheduling.state.lock();
        match scheduling.run_state {
            RunState::Blocking { cpu: owner } => {
                assert_eq!(owner, cpu, "blocking task returned on another CPU");
                scheduling.run_state = RunState::Blocked;
                None
            }
            RunState::WakePending { cpu: owner } => {
                assert_eq!(owner, cpu, "wake-pending task returned on another CPU");
                let target = if scheduling.cpu_affinity.allows_hart(cpu) {
                    cpu
                } else {
                    select_cpu(task, scheduling.cpu_affinity)
                };
                Some((target, scheduling.transition_to_ready(target)))
            }
            RunState::Preempting { cpu: owner } => {
                assert_eq!(owner, cpu, "preempting task returned on another CPU");
                let target_cpu = select_cpu(task, scheduling.cpu_affinity);
                Some((target_cpu, scheduling.transition_to_ready(target_cpu)))
            }
            RunState::StopPending {
                cpu: owner,
                transition,
            } => {
                assert_eq!(owner, cpu, "stopping task returned on another CPU");
                scheduling.run_state = RunState::Stopped {
                    resume: match transition {
                        StopTransition::Blocking => StopResume::Blocked,
                        StopTransition::Running
                        | StopTransition::Preempting
                        | StopTransition::WakePending => StopResume::Runnable,
                    },
                };
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
