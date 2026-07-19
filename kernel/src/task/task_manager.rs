use alloc::sync::Arc;

use crate::arch::context::KernelContext;
use crate::{
    cpu,
    fallible_tree::{FallibleMap, NodeSlot, VacantEntry},
    sync::{IrqMutex, LocalIrqGuard},
    task::{
        PendingSignal, Processor, RunState, StopResume, TaskControlBlock, WaitMembership,
        WaitResult,
        pid::{INIT_PID, PID_MAX, ProcessId},
        processor::{begin_preempt_running_task, enqueue_new_task},
        with_current_processor,
    },
    timer::{get_time_ns, get_time_us},
};

pub(in crate::task) mod advisory_lock;
mod affinity;
mod console_batch;
mod console_wait;
pub(super) mod context_switch;
mod deferred;
mod futex;
mod io_wait;
mod load_average;
mod parent_death;
mod pipe_wait;
mod policy;
mod process_exit;
mod process_group;
mod procfs;
mod resource_limit;
mod signal;
mod snapshot_staging;
pub(in crate::task) mod task_mutex_wait;
mod terminal_access;
mod thread_activation;
mod thread_clone;
mod thread_selector;
pub(in crate::task) mod timer_queue;
pub(in crate::task) mod vfork;
mod wait_child;
mod wait_key;
mod wait_publication;
mod wait_registry;

pub(crate) use affinity::{SchedulerAffinityError, scheduler_affinity};
pub(crate) use console_wait::{drain_terminal_input, wait_for_console};
use console_wait::{process_terminal_input, wake_console_waiters};
use context_switch::{schedule_with_task_context, switch_from_idle};
pub(crate) use deferred::dispatch_pending_deferred_work;
pub(in crate::task) use futex::futex_wake_with_key;
pub(crate) use futex::{FutexWaitError, futex_requeue, futex_wait, futex_wake};
pub(super) use io_wait::initialize_driver_io_wait;
pub(crate) use parent_death::parent_death_signal;
pub(crate) use pipe_wait::{
    create_notification_endpoints, create_pipe_endpoints, wait_for_pipe, wait_for_pipe_until,
};
pub(crate) use policy::{SchedulerNiceSelector, scheduler_nice, scheduler_rr_interval};
pub(crate) use policy::{
    SchedulerPolicyError, SchedulerPolicyRequest, scheduler_io_priority, scheduler_policy,
};
use process_exit::ProcessExitStatus;
pub(crate) use process_exit::{
    exit_current_group, exit_current_group_by_signal, exit_current_if_group_exiting,
    exit_current_thread,
};
pub(crate) use process_group::{
    ProcessGroupError, SetProcessGroupError, claim_controlling_terminal, create_session,
    process_group, session_id, set_process_group, set_terminal_foreground_group,
    terminal_foreground_group,
};
pub(in crate::task) use process_group::{current_process_group_is_orphaned, mark_process_exec};
pub(crate) use procfs::{KernelProcSource, SystemInfoSnapshot, system_info_snapshot};
pub(crate) use resource_limit::process_resource_limit;
use resource_limit::{ProcessSlotSnapshot, enforce_cpu_limit};
use signal::{ChildEvents, JobControlState};
pub(crate) use signal::{
    SignalSendError, send_kernel_thread_signal, send_kernel_thread_signal_info,
    send_process_signal, send_thread_signal, send_tid_signal, stop_current_process,
};
use signal::{complete_process_stop, send_kernel_process_signal, send_process_group_signal};
pub(crate) use terminal_access::{
    TerminalAccessError, check_terminal_access, hangup_terminal, publish_terminal_input_signals,
    resize_terminal,
};
pub(crate) use thread_clone::{ThreadCloneError, clone_current_thread};
pub(crate) use thread_selector::{parent_pid, thread_count};
use vfork::complete_vfork;
pub(crate) use vfork::{ProcessCloneError, fork_current_process, vfork_current_process};
use wait_child::take_child_waiters;
pub(crate) use wait_child::{
    WaitChildError, consume_child_status, release_child_status, wait_child,
};
use wait_key::IndexedWaitKind;
pub(crate) use wait_key::PollWaitKey;
use wait_registry::{CancelOutcome, WAIT_REGISTRY, arm_current as arm_indexed_wait};
enum ProcessState {
    Live(FallibleMap<usize, Arc<TaskControlBlock>>),
    Exited(ProcessExitStatus),
}

struct ThreadIndex {
    tgid: usize,
    created_children: FallibleMap<usize, ()>,
}

struct ProcessGroupIndex {
    members: FallibleMap<usize, ()>,
    exit_check_pending: bool,
    exit_check_next: Option<(usize, usize)>,
    was_orphaned_stopped: bool,
}

struct ProcessNode {
    parent: Option<usize>,
    // OWNER: graph 独占 creator Thread；只存 parent TGID 会在错误的 sibling exit 生成 pdeath signal。
    parent_thread: Option<usize>,
    children: FallibleMap<usize, ()>,
    session: usize,
    process_group: usize,
    group_slot: Option<NodeSlot<(usize, usize), ProcessGroupIndex>>,
    // 标记 exec point-of-no-return；缺少它会让 parent 在新映像生效后仍成功 setpgid。
    has_execed: bool,
    state: ProcessState,
    group_exit: Option<ProcessExitStatus>,
    job_control: JobControlState,
    exit_effects: u8,
    exit_effect_next: [Option<usize>; 2],
    // Exact count skips zero-pdeath Thread scans；pending/next/cursor resume the allocation-free queue.
    pdeath_enabled_threads: usize,
    pdeath_pending: bool,
    pdeath_next: Option<usize>,
    pdeath_cursor: usize,
    child_events: ChildEvents,
    child_waiters: FallibleMap<usize, Arc<TaskControlBlock>>,
    child_wait_claim: Option<wait_child::ChildWaitClaim>,
    vfork_parent: Option<Arc<TaskControlBlock>>,
}

struct ProcessGraph {
    next_pid: usize,
    processes_created: u64,
    nodes: FallibleMap<usize, ProcessNode>,
    threads: FallibleMap<usize, ThreadIndex>,
    groups: FallibleMap<(usize, usize), ProcessGroupIndex>,
    exit_group_head: Option<(usize, usize)>,
    exit_effect_heads: [Option<usize>; 2],
    pdeath_head: Option<usize>,
}

/// @description parent relation、live task 或最小 exit record 的唯一 process graph owner。
struct TaskManager {
    graph: IrqMutex<ProcessGraph>,
    timers: IrqMutex<timer_queue::TimerQueue>,
    load_average: load_average::LoadAverage,
    process_creation: IrqMutex<()>,
}

/// @description 在执行可能发布 thread-owned mapping 的构造前，先取得 TCB Arc storage。
/// @param out_of_memory Arc control block 分配失败时返回的领域错误。
/// @param build 仅在 Arc storage 已就绪后执行的未发布 TCB 构造事务。
/// @return 成功返回唯一 Arc-owned TCB；分配或构造失败不发布 task。
fn try_allocate_task<E>(
    out_of_memory: E,
    build: impl FnOnce() -> Result<TaskControlBlock, E>,
) -> Result<Arc<TaskControlBlock>, E> {
    let mut slot = Arc::<TaskControlBlock>::try_new_uninit().map_err(|_| out_of_memory)?;
    let task = build()?;
    Arc::get_mut(&mut slot)
        .expect("new task Arc must be uniquely owned")
        .write(task);
    // SAFETY: slot 是刚分配且未克隆的唯一 Arc；上一步完整写入一个 TaskControlBlock，
    // 此后不再通过 MaybeUninit 观察或析构同一 storage。
    Ok(unsafe { slot.assume_init() })
}

impl TaskManager {
    const fn new() -> Self {
        Self {
            graph: IrqMutex::new(ProcessGraph {
                next_pid: INIT_PID + 1,
                processes_created: 1,
                nodes: FallibleMap::new(),
                threads: FallibleMap::new(),
                groups: FallibleMap::new(),
                exit_group_head: None,
                exit_effect_heads: [None; 2],
                pdeath_head: None,
            }),
            timers: IrqMutex::new(timer_queue::TimerQueue::new()),
            load_average: load_average::LoadAverage::new(),
            process_creation: IrqMutex::new(()),
        }
    }

    fn add_init(&self, task: Arc<TaskControlBlock>) {
        let tgid = task.tgid();
        assert_eq!(tgid, INIT_PID);
        let mut threads = FallibleMap::new();
        threads
            .try_insert(task.tid(), task.clone())
            .expect("init thread node allocation failed");
        let mut members = FallibleMap::new();
        members
            .try_insert(tgid, ())
            .expect("init process-group member allocation failed");
        let group = FallibleMap::try_prepare(
            (INIT_PID, INIT_PID),
            ProcessGroupIndex {
                members,
                exit_check_pending: false,
                exit_check_next: None,
                was_orphaned_stopped: false,
            },
        )
        .expect("init process-group node allocation failed");
        let thread = FallibleMap::try_prepare(
            task.tid(),
            ThreadIndex {
                tgid,
                created_children: FallibleMap::new(),
            },
        )
        .expect("init thread-index node allocation failed");
        let process = FallibleMap::try_prepare(
            tgid,
            ProcessNode {
                parent: None,
                parent_thread: None,
                children: FallibleMap::new(),
                session: INIT_PID,
                process_group: INIT_PID,
                group_slot: None,
                has_execed: true,
                state: ProcessState::Live(threads),
                group_exit: None,
                job_control: JobControlState::Running,
                exit_effects: 0,
                exit_effect_next: [None; 2],
                pdeath_enabled_threads: 0,
                pdeath_pending: false,
                pdeath_next: None,
                pdeath_cursor: 0,
                child_events: ChildEvents::default(),
                child_waiters: FallibleMap::new(),
                child_wait_claim: None,
                vfork_parent: None,
            },
        )
        .expect("init process node allocation failed");
        let mut graph = self.graph.lock();
        graph.nodes.commit_vacant(process);
        graph.threads.commit_vacant(thread);
        graph.groups.commit_vacant(group);
        drop(graph);
        enqueue_new_task(task);
    }

    fn allocate_pid(&self) -> Option<ProcessId> {
        let mut graph = self.graph.lock();
        let pid = graph.next_pid;
        if pid > PID_MAX {
            return None;
        }
        graph.next_pid = pid + 1;
        Some(ProcessId::allocated(pid))
    }

    fn publish_thread(
        &self,
        tgid: usize,
        thread: Arc<TaskControlBlock>,
        prepared: VacantEntry<usize, Arc<TaskControlBlock>>,
        thread_index: VacantEntry<usize, ThreadIndex>,
    ) {
        let mut graph = self.graph.lock();
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("thread group missing from process graph");
        let ProcessState::Live(threads) = &mut node.state else {
            panic!("cannot publish thread into exited process");
        };
        debug_assert_eq!(prepared.value().tid(), thread.tid());
        threads.commit_vacant(prepared);
        debug_assert_eq!(thread_index.value().tgid, tgid);
        graph.threads.commit_vacant(thread_index);
    }

    /// @description best-effort TID store 完成后，把已发布 Thread 从 New 原子转为 Ready/Stopped。
    fn activate_thread(&self, tgid: usize, thread: Arc<TaskControlBlock>) {
        let graph = self.graph.lock();
        let node = graph
            .nodes
            .get(&tgid)
            .expect("published thread group disappeared before activation");
        assert!(matches!(node.state, ProcessState::Live(_)));
        let activation_state = match thread.scheduling.state.lock().run_state() {
            RunState::New => thread_activation::PreActivationState::New,
            RunState::Stopped {
                resume: StopResume::New,
            } => thread_activation::PreActivationState::StoppedNew,
            _ => thread_activation::PreActivationState::Activated,
        };
        let decision = thread_activation::new_thread_activation(
            activation_state,
            node.group_exit.is_some(),
            node.job_control == JobControlState::Running,
        );
        if decision.inherit_group_exit {
            // begin_group_exit 的首轮 SIGKILL snapshot 可能早于 child publication；任何
            // scheduler state 都必须继承该 consequence，parent-visible status 仍由 graph 决定。
            thread
                .queue_signal(core::iter::empty(), 9, PendingSignal::kernel())
                .expect("kernel SIGKILL must be valid");
        }
        match decision.transition {
            thread_activation::ActivationTransition::None => {}
            thread_activation::ActivationTransition::StopNew => {
                crate::task::processor::request_task_stop(&thread);
            }
            thread_activation::ActivationTransition::ReadyNew => {
                // graph owner 覆盖 job-control 决策到 Ready publication；否则并发 stop 可在
                // New→Ready 间穿过，使 clone child 绕过 group stop。
                enqueue_new_task(thread);
            }
            thread_activation::ActivationTransition::ResumeStoppedNew => {
                crate::task::processor::continue_stopped_task(thread.clone());
                enqueue_new_task(thread);
            }
        }
    }
}

// OWNER: task manager owns PID allocation, parent relation, live task/exit record and child waiter.
static TASK_MANAGER: TaskManager = TaskManager::new();

/// @description 发布 kernel 创建的唯一 init task。
///
/// @param task TGID 必须为 INIT_PID 且尚未进入 process graph。
/// @return 无返回值。
pub(super) fn add_init_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_init(task);
}

/// @description 为一次 ppoll 在多个 I/O source index 上发布唯一 wait registration。
///
/// @param keys 去重前的 Pipe/Console readiness keys。
/// @param deadline 可选 absolute monotonic timeout。
/// @param ready registration publication 后、全部 registry shard lock 外执行 readiness 复查。
/// @return source ready、timeout 或 signal interruption。
pub(crate) fn wait_for_poll(
    mut keys: alloc::vec::Vec<PollWaitKey>,
    deadline: Option<u64>,
    ready: impl FnOnce() -> bool,
) -> WaitResult {
    PollWaitKey::normalize(&mut keys);
    let task = current_task().expect("ppoll wait requires current task");
    let ticket = WAIT_REGISTRY.allocate_ticket();
    let prepared = ticket.prepare_poll(keys, deadline, task.clone());
    arm_indexed_wait(
        &task,
        prepared,
        || {
            if ready() {
                Some(WaitResult::Woken)
            } else if deadline.is_some_and(|value| value <= get_time_ns()) {
                Some(WaitResult::TimedOut)
            } else if task.has_deliverable_signal() {
                Some(WaitResult::Interrupted)
            } else {
                None
            }
        },
        WaitMembership::Poll,
    )
    .map_or_else(core::convert::identity, |prepared| prepared.suspend())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalWaitError {
    Again,
    Interrupted,
    OutOfMemory,
}

/// @description 在统一 wait registry 中等待并消费一个指定 pending signal。
///
/// @param mask 用户提供且已去除 SIGKILL/SIGSTOP 的 signal set。
/// @param deadline 可选 absolute monotonic deadline；None 表示无限等待。
/// @return 匹配 signal number 与 siginfo 来源。
/// @errors zero/到期 timeout 返回 Again；无关的可交付 signal 返回 Interrupted。
pub(crate) fn wait_for_signal(
    mask: u64,
    deadline: Option<u64>,
) -> Result<(usize, PendingSignal), SignalWaitError> {
    let task = current_task().expect("signal wait requires current task");
    loop {
        if let Some(signal) = task.take_pending_signal(mask) {
            return Ok(signal);
        }
        let ticket = WAIT_REGISTRY.allocate_ticket();
        let prepared = ticket.prepare_signal(mask, deadline, task.clone());
        let result = arm_indexed_wait(
            &task,
            prepared,
            || {
                if task.with_pending_signal(mask, || ()).is_some() {
                    Some(WaitResult::Woken)
                } else if deadline.is_some_and(|value| value <= get_time_ns()) {
                    Some(WaitResult::TimedOut)
                } else if task.has_deliverable_signal() {
                    Some(WaitResult::Interrupted)
                } else {
                    None
                }
            },
            WaitMembership::Signal,
        )
        .map_or_else(core::convert::identity, |prepared| prepared.suspend());
        match result {
            WaitResult::Woken => {}
            WaitResult::TimedOut => return Err(SignalWaitError::Again),
            WaitResult::Interrupted => return Err(SignalWaitError::Interrupted),
            WaitResult::OutOfMemory => return Err(SignalWaitError::OutOfMemory),
        }
    }
}

/// @description 用 Signal membership 等待 trap-return 可交付 signal，但不消费 pending bit。
///
/// @param deliverable_set sigsuspend 临时 mask 的补集。
/// @return signal 发布后返回；pending signal 留给唯一 trap delivery path。
pub(crate) fn wait_for_signal_delivery(deliverable_set: u64) -> WaitResult {
    let task = current_task().expect("signal delivery wait requires current task");
    let ticket = WAIT_REGISTRY.allocate_ticket();
    let prepared = ticket.prepare_signal(deliverable_set, None, task.clone());
    let result = arm_indexed_wait(
        &task,
        prepared,
        || {
            task.with_pending_signal(deliverable_set, || ())
                .map(|()| WaitResult::Woken)
        },
        WaitMembership::Signal,
    )
    .map_or_else(core::convert::identity, |prepared| prepared.suspend());
    assert_eq!(
        result,
        WaitResult::Woken,
        "sigsuspend has no timeout/cancellation path"
    );
    result
}

/// @description 在统一 wait registry 上阻塞到 absolute monotonic deadline。
///
/// @param deadline_ns absolute monotonic 纳秒 deadline。
/// @return deadline 已到或到期返回 `TimedOut`；signal cancellation 返回 `Interrupted`。
pub(crate) fn sleep_until(deadline_ns: u64) -> WaitResult {
    if deadline_ns <= get_time_ns() {
        return WaitResult::TimedOut;
    }
    let result = block_current_until(deadline_ns);
    assert_ne!(
        result,
        WaitResult::Woken,
        "deadline wait cannot complete through generic wake"
    );
    result
}

/// 获取并移除当前任务
pub(crate) fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(Processor::take_current)
}

/// 获取当前任务的引用
pub(crate) fn current_task() -> Option<Arc<TaskControlBlock>> {
    with_current_processor(|processor| processor.current.clone())
}

pub(crate) fn run_tasks() -> ! {
    with_current_processor(|_| {
        // Release 发布 local Processor 初始化；缺失时远端选核可能向尚未开始 drain 的 CPU 投递任务。
        cpu::mark_active();
    });
    loop {
        // 1. 关中断覆盖 deferred work、mailbox drain 和 task select，保证 idle 决策看到一致状态。
        let idle_irq = LocalIrqGuard::disable();
        scheduler_deferred_safe_point();
        with_current_processor(|processor| processor.drain_inbound_to_local());
        let task = with_current_processor(Processor::select_task);
        if let Some(task) = task {
            switch_from_idle(task);
            // guard 跨切换保活，使 continuation 在 local interrupt disabled 下完成 bookkeeping。
            drop(idle_irq);
            continue;
        }

        // 2. guard 保持 local IRQ 关闭直到 architecture seam 临时开中断并完成 WFI。固定的
        // WFI/resume PC 使 trap entry 能跳过已消费 edge 对应的 WFI，关闭 enable-to-WFI 窗口。
        // 3. seam 返回时 IRQ 仍关闭；guard 随后恢复原状态，下一轮再原子复查全部 scheduler state。
        crate::arch::interrupt::wait_with_local_irq_masked();
        drop(idle_irq);
    }
}

/// @description IRQ-closed scheduler safe point 的唯一 deferred dispatch seam。
pub(super) fn scheduler_deferred_safe_point() {
    dispatch_pending_deferred_work();
}

/// 挂起当前任务并运行下一个任务
pub(crate) fn suspend_current_and_run_next() {
    let Some(task) = current_task() else {
        return;
    };
    // 更新 CFS 使用的运行时间。
    let end_time = get_time_us();
    task.scheduling.policy.lock().finish_runtime(end_time);
    begin_preempt_running_task(&task);
    schedule_with_task_context(task);
}

/// @description 将当前 task 加入 indexed wait registry，并直接 handoff 给 runnable successor；
/// 没有 successor 时才进入 idle。
///
/// @param deadline 绝对 monotonic 纳秒 deadline。
/// @return task 被重新调度后返回 timeout 或 signal interruption 结果。
fn block_current_until(deadline: u64) -> WaitResult {
    let task = current_task().expect("deadline wait requires current task");
    let ticket = WAIT_REGISTRY.allocate_ticket();
    let prepared = ticket.prepare_deadline(deadline, task.clone());
    arm_indexed_wait(
        &task,
        prepared,
        || {
            task.has_deliverable_signal()
                .then_some(WaitResult::Interrupted)
        },
        WaitMembership::Deadline,
    )
    .map_or_else(core::convert::identity, |prepared| prepared.suspend())
}
