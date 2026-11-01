use crate::execution::sync::WaitResult;
use alloc::sync::Arc;

use crate::arch::context::KernelContext;
use crate::{
    execution::cpu,
    execution::sync::{IrqMutex, LocalIrqGuard},
    execution::task::{
        KernelThreadBody, PendingSignal, Processor, RunState, StopResume, TaskControlBlock,
        WaitMembership,
        pid::{INIT_PID, PidLinkSlot, PidRegistration, PreparedPid, ProcessId},
        processor::{begin_preempt_running_task, enqueue_new_task},
        with_current_processor,
    },
    execution::timer::{get_time_ns, get_time_us},
    fallible_tree::{FallibleMap, NodeSlot, VacantEntry},
    memory::MemoryError,
};

pub(in crate::execution::task) mod advisory_lock;
mod affinity;
mod capabilities;
mod console_batch;
mod console_wait;
pub(crate) use capabilities::capability_sets;
pub(super) mod context_switch;
mod deferred;
mod exec_drain;
mod exec_identity;
pub(in crate::execution::task) use exec_drain::ExecPreparation;
mod futex;
mod group_retirement;
mod io_wait;
mod load_average;
mod parent_death;
mod pid_namespace;
pub(in crate::execution::task) mod pipe_wait;
mod policy;
mod process_exit;
mod process_group;
mod process_retirement;
pub(in crate::execution::task) mod ptrace;
pub(crate) use ptrace::{PtraceError, PtraceRequest, ptrace_attach, ptrace_request, ptrace_target};
mod procfs;
mod resource_limit;
mod seccomp;
mod signal;
pub(in crate::execution::task) use signal::signal_action_for_group;
pub(in crate::execution::task) mod pidfd;
mod snapshot_staging;
pub(in crate::execution::task) mod task_wait;
mod terminal_access;
mod thread_activation;
mod thread_clone;
mod thread_selector;
pub(in crate::execution::task) mod timer_queue;
pub(in crate::execution::task) mod vfork;
mod wait_child;
pub(crate) use pidfd::{PidFdError, open_pidfd, pidfd_get_file, pidfd_identity};
mod wait_key;
mod wait_publication;
mod wait_registry;

pub(crate) use affinity::{SchedulerAffinityError, scheduler_affinity};
use console_wait::{process_terminal_input, wake_console_waiters};
use context_switch::{schedule_with_task_context, switch_from_idle};
pub(crate) use deferred::dispatch_pending_deferred_work;
pub(in crate::execution::task) use exec_identity::mark_process_exec;
pub(in crate::execution::task) use futex::futex_wake_with_key;
pub(crate) use futex::{FutexWaitError, futex_requeue, futex_wait, futex_wake, futex_wake_op};
pub(super) use io_wait::initialize_driver_io_wait;
pub(crate) use parent_death::parent_death_signal;
pub(crate) use policy::{SchedulerNiceSelector, scheduler_nice, scheduler_rr_interval};
pub(crate) use policy::{
    SchedulerPolicyError, SchedulerPolicyRequest, scheduler_io_priority, scheduler_policy,
};
use process_exit::ProcessExitStatus;
pub(super) use process_exit::exit_current_kernel_thread;
pub(crate) use process_exit::{
    exit_current_after_exec_failure, exit_current_group, exit_current_group_by_signal,
    exit_current_if_group_exiting, exit_current_thread, exit_current_thread_by_signal,
};
pub(in crate::execution::task) use process_group::current_process_group_is_orphaned;
pub(crate) use process_group::{
    ProcessGroupError, SetProcessGroupError, create_session, process_group, session_id,
    set_process_group,
};
use process_group::{claim_controlling_terminal, set_terminal_foreground_group};
pub(crate) use procfs::{KernelProcSource, SystemInfoSnapshot, system_info_snapshot};
pub(crate) use resource_limit::process_resource_limit;
pub(in crate::execution::task) use resource_limit::with_credential_transition;
use resource_limit::{ThreadSlotSnapshot, enforce_cpu_limit};
pub(crate) use seccomp::{enable_strict_seccomp, install_seccomp_filter};
use signal::{ChildEvents, JobControlState};
pub(crate) use signal::{
    PidSignalScope, SignalSendError, notify_signal_waiters, send_kernel_thread_signal,
    send_pid_signal, send_process_signal, send_queued_process_signal, send_queued_thread_signal,
    send_thread_signal, send_tid_signal, stop_current_process,
};
use signal::{complete_process_stop, send_kernel_process_signal, send_process_group_signal};
pub(super) use terminal_access::install_job_control;
pub(crate) use thread_clone::{ThreadCloneError, clone_current_thread};
pub(crate) use thread_selector::{is_single_threaded, parent_pid, robust_list_head};

pub(crate) use vfork::{ProcessCloneError, clone_current_process};
use wait_child::take_child_waiters;
pub(crate) use wait_child::{
    ChildWaitClass, ChildWaitFilter, ChildWaitSelector, WaitChildError, child_resource_usage,
    child_status_info, child_status_pid, consume_child_status, release_child_status, wait_child,
    wait_child_events,
};
use wait_key::IndexedWaitKind;
pub(crate) use wait_key::PollWaitKey;
use wait_registry::{CancelOutcome, WAIT_REGISTRY, arm_current as arm_indexed_wait};
enum ProcessState {
    Live(FallibleMap<usize, Arc<TaskControlBlock>>),
    Exited(ProcessExitStatus, ExitRetirement),
}

/// Exit 的唯一回收决定；active wait claim 完成前不得释放其 event owner。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExitRetirement {
    /// 所有执行体已摘除，但实际 Thread retirement 仍持 MM/vfork 清理 obligation。
    Retiring,
    Waitable,
    /// 实际 retirement 已完成，leader exit 尚须先交给 tracer，再通知/回收 real parent。
    Traced,
    AfterClaim,
}

struct ThreadIndex {
    pid: PidRegistration,
    // 非 leader 的 actual PID readiness source；leader 的同一 identity 只归 ProcessNode。
    pidfd_source: alloc::sync::Weak<crate::fs::pidfs::PidFdSource>,
    tgid: usize,
    // OWNER: 不可返回的退出已开始，但 cleanup 仍可阻塞；TSYNC 跳过而 scheduler 保留 current。
    exit_in_progress: bool,
    // Live map 摘除后移交唯一实际 execution admission，直到 MM/vfork completion 后才释放。
    // 只在退出清理期间持 TCB；其 MM/files/ns 已退休，PID 查询仍能授权原 Thread，不能借 sibling。
    retiring_task: Option<Arc<TaskControlBlock>>,
    created_children: FallibleMap<usize, ()>,
    // OWNER: vfork completion 属于原始 child Thread；sibling 不继承，exit 先移出 token。
    vfork_parent: Option<Arc<TaskControlBlock>>,
}

struct ProcessGroupIndex {
    pgid: PidRegistration,
    sid: PidRegistration,
    members: FallibleMap<usize, ()>,
    exit_check_pending: bool,
    exit_check_next: Option<usize>,
    was_orphaned_stopped: bool,
}

struct ProcessNode {
    // OWNER: graph 与 live Thread 共享唯一 Process；zombie 无 TCB/mm/files/ns membership edge。
    process: Arc<super::model::Process>,
    pid: PidRegistration,
    // actual leader PID 的 lazy notification identity；Weak 不延长 inode/OFD/执行体寿命。
    pidfd_source: alloc::sync::Weak<crate::fs::pidfs::PidFdSource>,
    namespace_exit: Option<Arc<pid_namespace::NamespaceExit>>,
    // OWNER: original leader 的同一可重定向 credential handle，保留到 zombie reap；无 TCB/mm/files edge。
    leader_credentials: super::model::LeaderCredentialsHandle,
    parent: Option<usize>,
    // OWNER: graph 独占 creator Thread；只存 parent TGID 会在错误的 sibling exit 生成 pdeath signal。
    parent_thread: Option<usize>,
    // native group-leader exit_signal；零仍须等待，reparent/exec 改为 SIGCHLD。
    exit_signal: u8,
    // child relation 冻结 real parent 的 exec epoch；父映像改变后 notification 强制 SIGCHLD。
    parent_exec_generation: u64,
    children: FallibleMap<usize, ()>,
    session: usize,
    process_group: usize,
    group_slot: Option<NodeSlot<usize, ProcessGroupIndex>>,
    // 标记 exec point-of-no-return；缺少它会让 parent 在新映像生效后仍成功 setpgid。
    exec_generation: u64,
    state: ProcessState,
    group_exit: Option<ProcessExitStatus>,
    // OWNER: 唯一 exec preparation/drain claim；缺少它会让并发 exec 覆盖或漏掉新 sibling。
    exec: Option<exec_drain::ExecState>,
    job_control: JobControlState,
    exit_effects: u8,
    exit_effect_next: [Option<usize>; 2],
    // Exact count skips zero-pdeath Thread scans；pending/next/cursor resume the allocation-free queue.
    pdeath_enabled_threads: usize,
    pdeath_pending: bool,
    pdeath_next: Option<usize>,
    pdeath_cursor: usize,
    child_events: ChildEvents,
    // OWNER: 此 tracer group 的受跟踪 TID membership；状态只归 graph.tracees。
    // wait/exit 不得因系统其他 debugger 的目标数量而扫描整个 tracee registry。
    ptracees: FallibleMap<usize, ()>,
    child_waiters: FallibleMap<usize, Arc<TaskControlBlock>>,
    child_wait_claim: Option<wait_child::ChildWaitClaim>,
}

struct ProcessGraph {
    // OWNER: graph 独占逐 Thread tracer relation、停止与 wait claim；不保活目标 TCB/MM。
    // 缺少独立 relation 会把 ptrace-stop 错当整个进程的 job-control stop。
    tracees: FallibleMap<usize, ptrace::Tracee>,
    next_pid: usize,
    processes_created: u64,
    nodes: FallibleMap<usize, ProcessNode>,
    threads: FallibleMap<usize, ThreadIndex>,
    // OWNER: 无用户 Process 的实际 kernel TCB membership；PID registration 仍归同一 KernelThread。
    // 此 index 保活执行体至实际 exit，len 与 user TID index 一起投影系统任务数，不维护计数镜像。
    kernel_threads: FallibleMap<usize, Arc<TaskControlBlock>>,
    groups: FallibleMap<usize, ProcessGroupIndex>,
    exit_group_head: Option<usize>,
    exit_effect_heads: [Option<usize>; 2],
    pdeath_head: Option<usize>,
}

impl ProcessGraph {
    /// 系统级任务总数直接来自实际 user/kernel execution memberships；不计 boot idle。
    fn live_task_count(&self) -> usize {
        self.threads.len() + self.kernel_threads.len()
    }
}

/// parent relation、live task 或最小 exit record 的唯一 process graph owner。
struct ProcessTable {
    graph: IrqMutex<ProcessGraph>,
    timers: IrqMutex<timer_queue::TimerQueue>,
    load_average: load_average::LoadAverage,
    process_creation: IrqMutex<()>,
}

/// 在执行可能发布 thread-owned mapping 的构造前，先取得 TCB Arc storage。
///
/// # Parameters
///
/// - `out_of_memory`: Arc control block 分配失败时返回的领域错误。
/// - `build`: 仅在 Arc storage 已就绪后执行的未发布 TCB 构造事务。
///
/// # Returns
///
/// 成功返回唯一 Arc-owned TCB；分配或构造失败不发布 task。
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

impl ProcessTable {
    const fn new() -> Self {
        Self {
            graph: IrqMutex::new(ProcessGraph {
                tracees: FallibleMap::new(),
                next_pid: INIT_PID + 1,
                processes_created: 1,
                nodes: FallibleMap::new(),
                threads: FallibleMap::new(),
                kernel_threads: FallibleMap::new(),
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

    fn add_init(&self, task: Arc<TaskControlBlock>, pid: &mut PreparedPid) {
        let process_pid = pid.take_registration();
        let thread_pid =
            process_pid.clone_in(PidLinkSlot::try_new().expect("init TID link allocation failed"));
        let pgid =
            process_pid.clone_in(PidLinkSlot::try_new().expect("init PGID link allocation failed"));
        let sid =
            process_pid.clone_in(PidLinkSlot::try_new().expect("init SID link allocation failed"));
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
            INIT_PID,
            ProcessGroupIndex {
                pgid,
                sid,
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
                pid: thread_pid,
                pidfd_source: alloc::sync::Weak::new(),
                tgid,
                exit_in_progress: false,
                retiring_task: None,
                created_children: FallibleMap::new(),
                vfork_parent: None,
            },
        )
        .expect("init thread-index node allocation failed");
        let process = FallibleMap::try_prepare(
            tgid,
            ProcessNode {
                process: task.process().clone(),
                pid: process_pid,
                pidfd_source: alloc::sync::Weak::new(),
                namespace_exit: None,
                leader_credentials: task.leader_credentials_handle(),
                parent: None,
                parent_thread: None,
                exit_signal: syscall_abi::signal::SIGCHLD as u8,
                parent_exec_generation: 0,
                children: FallibleMap::new(),
                session: INIT_PID,
                process_group: INIT_PID,
                group_slot: None,
                exec_generation: 1,
                state: ProcessState::Live(threads),
                group_exit: None,
                exec: None,
                job_control: JobControlState::Running,
                exit_effects: 0,
                exit_effect_next: [None; 2],
                pdeath_enabled_threads: 0,
                pdeath_pending: false,
                pdeath_next: None,
                pdeath_cursor: 0,
                child_events: ChildEvents::default(),
                ptracees: FallibleMap::new(),
                child_waiters: FallibleMap::new(),
                child_wait_claim: None,
            },
        )
        .expect("init process node allocation failed");
        let mut graph = self.graph.lock();
        pid.publish()
            .expect("boot initial PID namespace remains live");
        graph.nodes.commit_vacant(process);
        graph.threads.commit_vacant(thread);
        graph.groups.commit_vacant(group);
        drop(graph);
        enqueue_new_task(task);
    }

    fn allocate_key(&self) -> Option<usize> {
        let mut graph = self.graph.lock();
        let key = graph.next_pid;
        graph.next_pid = key.checked_add(1)?;
        Some(key)
    }
    fn allocate_pid(
        &self,
        namespace: Arc<crate::namespace::PidNamespace>,
        requested: &[i32],
        authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<PreparedPid, crate::namespace::PidAllocationError> {
        let key = self
            .allocate_key()
            .ok_or(crate::namespace::PidAllocationError::Exhausted)?;
        // graph guard 只领取稳定 key；实际 namespace node/ladder/link allocation 均在锁外。
        PreparedPid::try_new(key, namespace, requested, authority)
    }

    fn publish_thread(
        &self,
        creator: &TaskControlBlock,
        thread: Arc<TaskControlBlock>,
        prepared: VacantEntry<usize, Arc<TaskControlBlock>>,
        thread_index: VacantEntry<usize, ThreadIndex>,
        pid: &mut PreparedPid,
    ) -> Result<super::model::RetiredSecurity, crate::namespace::PidAllocationError> {
        let mut graph = self.graph.lock();
        pid.publish()?;
        let tgid = creator.tgid();
        let retired = thread
            .security_owner()
            .inherit_from(creator.security_owner());
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("thread group missing from process graph");
        let ProcessState::Live(threads) = &mut node.state else {
            panic!("cannot publish thread into exited process");
        };
        creator.thread_retirements().publish_thread();
        debug_assert_eq!(prepared.value().tid(), thread.tid());
        threads.commit_vacant(prepared);
        debug_assert_eq!(thread_index.value().tgid, tgid);
        graph.threads.commit_vacant(thread_index);
        // Linux total_forks 包含 CLONE_THREAD；只在实际 PID/TCB publication 成功时记录。
        graph.processes_created = graph.processes_created.saturating_add(1);
        Ok(retired)
    }

    /// best-effort TID store 完成后，把已发布 Thread 从 New 原子转为 Ready/Stopped。
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
            node.group_exit.is_some()
                || node
                    .exec
                    .as_ref()
                    .is_some_and(|exec| exec.kills(thread.tid())),
            node.job_control == JobControlState::Running,
        );
        if decision.inherit_group_exit {
            // begin_group_exit 的首轮 SIGKILL snapshot 可能早于 child publication；任何
            // scheduler state 都必须继承该 consequence，parent-visible status 仍由 graph 决定。
            thread.queue_group_exit_signal();
        }
        match decision.transition {
            thread_activation::ActivationTransition::None => {}
            thread_activation::ActivationTransition::StopNew => {
                crate::execution::task::processor::request_task_stop(&thread);
            }
            thread_activation::ActivationTransition::ReadyNew => {
                // graph owner 覆盖 job-control 决策到 Ready publication；否则并发 stop 可在
                // New→Ready 间穿过，使 clone child 绕过 group stop。
                enqueue_new_task(thread);
            }
            thread_activation::ActivationTransition::ResumeStoppedNew => {
                crate::execution::task::processor::continue_stopped_task(thread.clone());
                enqueue_new_task(thread);
            }
        }
    }
}

// OWNER: task manager owns PID allocation, parent relation, live task/exit record and child waiter.
static PROCESS_TABLE: ProcessTable = ProcessTable::new();

/// 发布 kernel 创建的唯一 init task。
///
/// # Parameters
///
/// - `task`: TGID 必须为 INIT_PID 且尚未进入 process graph。
pub(super) fn add_init_task(task: Arc<TaskControlBlock>, pid: &mut PreparedPid) {
    PROCESS_TABLE.add_init(task, pid);
}

/// 创建并调度一个只在内核态运行的线程。
///
/// 内核线程与用户任务共享实际 initial PID namespace 编号分配与 identity。它没有用户
/// Process/信号/rlimit；graph 的实际 kernel execution index 拥有 TCB admission，
/// 系统级 task/fork 统计与 proc/kill 投影包含它；boot worker 忽略信号，无用户 wait child 关系。资源退休
/// 仍能以真实 current TGID 更新 IPC 元数据。编号在实际 exit 归还，observer Arc 不延长占用。
///
/// # Parameters
///
/// - `name`: 诊断名称。
/// - `body`: 永不返回的线程主体；可以阻塞在 `TaskMutex`、`TaskEvent`、DriverIo 与 `sleep_until`。
///
/// # Errors
///
/// TID 耗尽或 kernel stack 分配失败时返回 `OutOfMemory`，不发布任何 scheduler state。
pub(crate) fn spawn_kernel_thread(
    name: &'static str,
    body: KernelThreadBody,
) -> Result<(), MemoryError> {
    let authority = crate::namespace::NamespaceAuthority {
        namespace: crate::namespace::initial_user(),
        effective_uid: 0,
        effective_gid: 0,
        // 无 requested PID，分配不做 capability 豁免；这份输入只说明真实 boot owner。
        effective_capabilities: 0,
    };
    let mut pid = PROCESS_TABLE
        .allocate_pid(crate::namespace::initial_pid(), &[], &authority)
        .map_err(|_| MemoryError::OutOfMemory)?;
    let registration = pid.take_registration();
    let task = try_allocate_task(MemoryError::OutOfMemory, || {
        TaskControlBlock::new_kernel_thread(registration, name, body)
    })?;
    let membership =
        FallibleMap::try_prepare(task.tid(), task.clone()).map_err(|_| MemoryError::OutOfMemory)?;
    {
        let mut graph = PROCESS_TABLE.graph.lock();
        pid.publish().map_err(|_| MemoryError::OutOfMemory)?;
        graph.kernel_threads.commit_vacant(membership);
        graph.processes_created = graph.processes_created.saturating_add(1);
    }
    // namespace reservation storage 与未发表失败任务均在 graph guard 外析构。
    drop(pid);
    enqueue_new_task(task);
    Ok(())
}

/// 为一次 ppoll 在多个 I/O source index 上发布唯一 wait registration。
///
/// # Parameters
///
/// - `keys`: 去重前的 Pipe/Console readiness keys。
/// - `deadline`: 可选 absolute monotonic timeout。
/// - `ready`: registration publication 后、全部 registry shard lock 外执行 readiness 复查。
///
/// # Returns
///
/// source ready、timeout 或 signal interruption。
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

/// 在统一 wait registry 中等待并消费一个指定 pending signal。
///
/// # Parameters
///
/// - `mask`: 用户提供且已去除 SIGKILL/SIGSTOP 的 signal set。
/// - `deadline`: 可选 absolute monotonic deadline；None 表示无限等待。
///
/// # Returns
///
/// 匹配 signal number 与 siginfo 来源。
///
/// # Errors
///
/// zero/到期 timeout 返回 Again；无关的可交付 signal 返回 Interrupted。
pub(crate) fn wait_for_signal(
    mask: u64,
    deadline: Option<u64>,
) -> Result<(usize, PendingSignal), SignalWaitError> {
    let task = current_task().expect("signal wait requires current task");
    loop {
        if let Some((signal, info, _)) = task.take_pending_signal(mask) {
            return Ok((signal, info));
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

/// 用 Signal membership 等待 trap-return 可交付 signal，但不消费 pending bit。
///
/// # Parameters
///
/// - `deliverable_set`: sigsuspend 临时 mask 的补集。
///
/// # Returns
///
/// 匹配 signal 返回 Woken，另一可投递 signal 取消等待时返回 Interrupted；
/// pending signal 都留给唯一 trap delivery path。
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
    assert!(
        matches!(result, WaitResult::Woken | WaitResult::Interrupted),
        "sigsuspend has no timeout/OOM completion after publication"
    );
    result
}

/// 在统一 wait registry 上阻塞到 absolute monotonic deadline。
///
/// # Parameters
///
/// - `deadline_ns`: absolute monotonic 纳秒 deadline。
///
/// # Returns
///
/// deadline 已到或到期返回 `TimedOut`；signal cancellation 返回 `Interrupted`。
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

/// 实际 current TGID；Processor 无 scheduled Task 时执行的是 boot/idle PID0。
/// PID0 在 namespace boot 已构造，绝不是失败路径临时制造的 identity。
pub(in crate::execution::task) fn current_tgid_identity() -> Arc<crate::namespace::PidIdentity> {
    current_task().map_or_else(crate::namespace::idle_pid_identity, |task| {
        task.tgid_identity().clone()
    })
}

pub(crate) fn run_tasks() -> ! {
    with_current_processor(|_| {
        // Release 发布 local Processor 初始化；缺失时远端选核可能向尚未开始 drain 的 CPU 投递任务。
        cpu::mark_active();
    });
    // CPU0 保留低成本 housekeeping/liveness tick；其余 CPU 的 tick 只在运行 task 时用于抢占。
    // timer queue 仍由 ProcessTable 唯一拥有；该 stack-local token 只避免每轮 spurious wake 重写 CSR。
    let keeps_housekeeping_tick = cpu::current_id() == cpu::boot_id();
    let mut local_tick_armed = true;
    loop {
        // 1. 关中断覆盖 deferred work、mailbox drain 和 task select，保证 idle 决策看到一致状态。
        let idle_irq = LocalIrqGuard::disable();
        scheduler_deferred_safe_point();
        with_current_processor(|processor| processor.drain_inbound_to_local());
        let task = with_current_processor(Processor::select_task);
        if let Some(task) = task {
            if !local_tick_armed {
                crate::execution::timer::resume_local_idle_tick();
                local_tick_armed = true;
            }
            switch_from_idle(task);
            // guard 跨切换保活，使 continuation 在 local interrupt disabled 下完成 bookkeeping。
            drop(idle_irq);
            continue;
        }

        // 2. 非 boot idle CPU 不承担 housekeeping liveness；关闭其本地周期 tick，避免 100Hz tick
        // 把每个空闲 vCPU 都从 HVF 唤醒。远端 runnable publication 的 IPI 仍可唤醒 WFI。
        if !keeps_housekeeping_tick && local_tick_armed {
            crate::execution::timer::suspend_local_idle_tick();
            local_tick_armed = false;
        }

        // 3. guard 保持 local IRQ 关闭直到 architecture seam 临时开中断并完成 WFI。固定的
        // WFI/resume PC 使 trap entry 能跳过已消费 edge 对应的 WFI，关闭 enable-to-WFI 窗口。
        // 4. seam 返回时 IRQ 仍关闭；guard 随后恢复原状态，下一轮再原子复查全部 scheduler state。
        crate::arch::interrupt::wait_with_local_irq_masked();
        drop(idle_irq);
    }
}

/// IRQ-closed scheduler safe point 的唯一 deferred dispatch seam。
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

/// 将当前 task 加入 indexed wait registry，并直接 handoff 给 runnable successor；
/// 没有 successor 时才进入 idle。
///
/// # Parameters
///
/// - `deadline`: 绝对 monotonic 纳秒 deadline。
///
/// # Returns
///
/// task 被重新调度后返回 timeout 或 signal interruption 结果。
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
