use core::sync::atomic::Ordering;

use alloc::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use lazy_static::lazy_static;

use crate::{
    arch::hart::{self, hart_id},
    sync::{IrqMutex, LocalIrqGuard},
    task::{
        PendingSignal, Processor, RunState, TaskControlBlock, WaitMembership, WaitResult,
        context::TaskContext,
        pid::{INIT_PID, ProcessId},
        processor::{begin_preempt_running_task, enqueue_new_task},
        with_current_processor,
    },
    timer::{get_time_ns, get_time_us},
};

pub(in crate::task) mod advisory_lock;
mod affinity;
mod context_switch;
mod deferred;
mod futex;
mod load_average;
mod pipe_wait;
mod policy;
mod process_exit;
mod process_group;
mod procfs;
mod resource_limit;
mod signal;
mod terminal_access;
mod thread_clone;
mod thread_selector;
pub(in crate::task) mod vfork;
mod wait_child;
mod wait_key;
mod wait_registry;

pub(crate) use affinity::{SchedulerAffinityError, scheduler_affinity};
use context_switch::{prepare_current_block, schedule_with_task_context};
pub(crate) use deferred::{dispatch_pending_deferred_work, real_timer, set_real_timer};
pub(crate) use futex::{FutexWaitError, futex_requeue, futex_wait, futex_wake};
pub(crate) use pipe_wait::{create_pipe_endpoints, wait_for_pipe};
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
use resource_limit::{check_process_slot, enforce_cpu_limit};
use signal::{ChildEvents, JobControlState};
pub(crate) use signal::{
    SignalSendError, send_kernel_thread_signal, send_process_signal, send_thread_signal,
    send_tid_signal, stop_current_process,
};
use signal::{complete_process_stop, send_kernel_process_signal, send_process_group_signal};
pub(crate) use terminal_access::{TerminalAccessError, check_terminal_access};
pub(crate) use thread_clone::{ThreadCloneError, clone_current_thread};
pub(crate) use thread_selector::{parent_pid, thread_count};
use vfork::complete_vfork;
pub(crate) use vfork::{fork_current_process, vfork_current_process};
use wait_child::take_child_waiters;
pub(crate) use wait_child::{
    WaitChildError, consume_child_status, release_child_status, wait_child,
};
use wait_key::IndexedWaitKind;
pub(crate) use wait_key::PollWaitKey;
use wait_registry::INDEXED_WAIT_QUEUE;
enum ProcessState {
    Live(BTreeMap<usize, Arc<TaskControlBlock>>),
    Exited(ProcessExitStatus),
}

struct ProcessNode {
    parent: Option<usize>,
    session: usize,
    process_group: usize,
    // 标记 exec point-of-no-return；缺少它会让 parent 在新映像生效后仍成功 setpgid。
    has_execed: bool,
    state: ProcessState,
    group_exit: Option<ProcessExitStatus>,
    job_control: JobControlState,
    child_events: ChildEvents,
    // OWNER: parent Process node owns every Thread currently waiting for a child event. A single
    // waiter slot makes concurrent waitpid either overwrite membership or return non-Linux EAGAIN.
    child_waiters: BTreeMap<usize, Arc<TaskControlBlock>>,
    // OWNER: process graph 独占 child event claim；copyout 成功才消费，EFAULT 释放。缺失该
    // claim 时两个 parent Thread 可同时返回同一 zombie，并由第二次 reap 触发状态分裂。
    child_wait_claim: Option<wait_child::ChildWaitClaim>,
    // child node 唯一持有 suspended vfork parent；缺失该 owner 会在 exec/exit 边界丢唤醒。
    vfork_parent: Option<Arc<TaskControlBlock>>,
}

struct ProcessGraph {
    next_pid: usize,
    processes_created: u64,
    nodes: BTreeMap<usize, ProcessNode>,
}

/// @description parent relation、live task 或最小 exit record 的唯一 process graph owner。
struct TaskManager {
    graph: IrqMutex<ProcessGraph>,
    // OWNER: RealTimerQueue 独占 record+deadline index；graph → timer 锁序把 TGID lifecycle
    // 与 set/get/exit 串行化。缺失 exit cleanup 会向已经退出的 Process 投递 stale SIGALRM。
    real_timers: IrqMutex<deferred::RealTimerQueue>,
    load_average: load_average::LoadAverage,
    // OWNER: clone/fork/vfork 从 RLIMIT_NPROC 检查到 graph publish 的唯一串行化锁。
    // 缺失它会让并发创建者同时通过同一剩余 slot，越过 Process soft limit。
    process_creation: IrqMutex<()>,
}

impl TaskManager {
    fn new() -> Self {
        Self {
            graph: IrqMutex::new(ProcessGraph {
                next_pid: INIT_PID + 1,
                processes_created: 1,
                nodes: BTreeMap::new(),
            }),
            real_timers: IrqMutex::new(deferred::RealTimerQueue::new()),
            load_average: load_average::LoadAverage::new(),
            process_creation: IrqMutex::new(()),
        }
    }

    fn add_init(&self, task: Arc<TaskControlBlock>) {
        let tgid = task.tgid();
        assert_eq!(tgid, INIT_PID);
        let mut threads = BTreeMap::new();
        threads.insert(task.tid(), task.clone());
        let previous = self.graph.lock().nodes.insert(
            tgid,
            ProcessNode {
                parent: None,
                session: INIT_PID,
                process_group: INIT_PID,
                has_execed: true,
                state: ProcessState::Live(threads),
                group_exit: None,
                job_control: JobControlState::Running,
                child_events: ChildEvents::default(),
                child_waiters: BTreeMap::new(),
                child_wait_claim: None,
                vfork_parent: None,
            },
        );
        assert!(previous.is_none(), "init inserted twice");
        enqueue_new_task(task);
    }

    fn allocate_pid(&self) -> ProcessId {
        let mut graph = self.graph.lock();
        let pid = graph.next_pid;
        graph.next_pid = graph
            .next_pid
            .checked_add(1)
            .expect("PID namespace exhausted");
        ProcessId::allocated(pid)
    }

    fn publish_thread(&self, tgid: usize, thread: Arc<TaskControlBlock>) -> bool {
        let mut graph = self.graph.lock();
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("thread group missing from process graph");
        let stopping = node.group_exit.is_none() && node.job_control != JobControlState::Running;
        let ProcessState::Live(threads) = &mut node.state else {
            panic!("cannot publish thread into exited process");
        };
        assert!(threads.insert(thread.tid(), thread.clone()).is_none());
        if stopping {
            crate::task::processor::request_task_stop(&thread);
        }
        stopping
    }
}

lazy_static! {
    // OWNER: task manager owns PID allocation, parent relation, live task/exit record and child waiter.
    static ref TASK_MANAGER: TaskManager = TaskManager::new();
}

/// @description 发布 kernel 创建的唯一 init task。
///
/// @param task TGID 必须为 INIT_PID 且尚未进入 process graph。
/// @return 无返回值。
pub(super) fn add_init_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.add_init(task);
}

fn process_terminal_input() {
    let terminal = {
        let graph = TASK_MANAGER.graph.lock();
        graph.nodes.values().find_map(|node| {
            let ProcessState::Live(threads) = &node.state else {
                return None;
            };
            threads.values().next().map(|task| task.terminal())
        })
    };
    let Some(terminal) = terminal else {
        return;
    };
    if drain_terminal_input(&terminal).is_err() {
        debug!("TTY line discipline failed to drain UART input");
    }
}

/// @description 将指定 Terminal 的 raw input 送入 line discipline 并投递 foreground signals。
///
/// @param terminal console OFD 与 Process 共享的唯一 TTY owner。
/// @return drain 成功返回 `Ok(())`；设备或固定 queue 失败返回 `Err(())`。
pub(crate) fn drain_terminal_input(terminal: &crate::fs::Terminal) -> Result<(), ()> {
    let signals = terminal.drain_input().map_err(|_| ())?;
    let Some(pgid) = terminal.signal_target_group() else {
        return Ok(());
    };
    for signal in 1..=64 {
        if signals & (1u64 << (signal - 1)) != 0 {
            send_process_group_signal(pgid, signal);
        }
    }
    Ok(())
}

/// @description 在统一 wait registry 中阻塞当前 console reader，封闭 read/enqueue IRQ race。
///
/// @param deadline VTIME 导出的 absolute monotonic deadline；无超时时为 None。
/// @param input_ready 在 registry owner lock 内复查 UART ring 的短闭包。
/// @return 输入已到达/IRQ 唤醒返回 `Woken`，到期返回 `TimedOut`，signal cancellation 返回 `Interrupted`。
pub(crate) fn wait_for_console(
    deadline: Option<u64>,
    input_ready: impl FnOnce() -> bool,
) -> WaitResult {
    let task = current_task().expect("console wait requires current task");
    let queue = INDEXED_WAIT_QUEUE.lock();
    if input_ready() {
        return WaitResult::Woken;
    }
    if deadline.is_some_and(|value| value <= get_time_ns()) {
        return WaitResult::TimedOut;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    prepare_current_block(&task, queue, |queue, current| {
        let wait_id = queue.insert_console(deadline, current);
        WaitMembership::Console(wait_id)
    })
    .suspend()
}

fn wake_console_waiters() -> usize {
    const INPUT: i16 = 0x001;
    let mut waiters = alloc::vec::Vec::new();
    let mut wake_groups = BTreeSet::new();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    while let Some((wait_id, entry, group)) = queue.take_console(false, INPUT, &wake_groups) {
        if let Some(group) = group {
            wake_groups.insert(group);
        }
        waiters.push((wait_id, entry));
    }
    if let Some((wait_id, entry, _)) = queue.take_console(true, INPUT, &wake_groups) {
        waiters.push((wait_id, entry));
    }
    drop(queue);
    let mut count = 0;
    for (wait_id, entry) in waiters {
        let woke = match entry.kind {
            IndexedWaitKind::Console => {
                crate::task::processor::wake_console_task(entry.task, wait_id, WaitResult::Woken)
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Woken)
            }
            _ => panic!("console index contains non-console wait"),
        };
        if woke {
            count += 1;
        }
    }
    count
}

/// @description 为一次 ppoll 在多个 I/O source index 上发布唯一 wait registration。
///
/// @param keys 去重前的 Pipe/Console readiness keys。
/// @param deadline 可选 absolute monotonic timeout。
/// @param ready 在 registry owner lock 内清理内部 edge token 并执行无阻塞 level readiness 复查。
/// @return source ready、timeout 或 signal interruption。
pub(crate) fn wait_for_poll(
    mut keys: alloc::vec::Vec<PollWaitKey>,
    deadline: Option<u64>,
    ready: impl FnOnce() -> bool,
) -> WaitResult {
    PollWaitKey::normalize(&mut keys);
    let task = current_task().expect("ppoll wait requires current task");
    let queue = INDEXED_WAIT_QUEUE.lock();
    if ready() {
        return WaitResult::Woken;
    }
    if deadline.is_some_and(|value| value <= get_time_ns()) {
        return WaitResult::TimedOut;
    }
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    prepare_current_block(&task, queue, |queue, current| {
        let wait_id = queue.insert_poll(keys, deadline, current);
        WaitMembership::Poll(wait_id)
    })
    .suspend()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalWaitError {
    Again,
    Interrupted,
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
        let queue = INDEXED_WAIT_QUEUE.lock();
        if let Some(signal) = task.take_pending_signal(mask) {
            return Ok(signal);
        }
        if deadline.is_some_and(|value| value <= get_time_ns()) {
            return Err(SignalWaitError::Again);
        }
        if task.has_deliverable_signal() {
            return Err(SignalWaitError::Interrupted);
        }

        let result = prepare_current_block(&task, queue, |queue, current| {
            let wait_id = queue.insert_signal(mask, deadline, current);
            WaitMembership::Signal(wait_id)
        })
        .suspend();
        match result {
            WaitResult::Woken => {}
            WaitResult::TimedOut => return Err(SignalWaitError::Again),
            WaitResult::Interrupted => return Err(SignalWaitError::Interrupted),
        }
    }
}

/// @description 用 Signal membership 等待 trap-return 可交付 signal，但不消费 pending bit。
///
/// @param deliverable_set sigsuspend 临时 mask 的补集。
/// @return signal 发布后返回；pending signal 留给唯一 trap delivery path。
pub(crate) fn wait_for_signal_delivery(deliverable_set: u64) {
    let task = current_task().expect("signal delivery wait requires current task");
    let queue = INDEXED_WAIT_QUEUE.lock();
    if task.with_pending_signal(deliverable_set, || ()).is_some() {
        return;
    }
    let result = prepare_current_block(&task, queue, |queue, current| {
        let wait_id = queue.insert_signal(deliverable_set, None, current);
        WaitMembership::Signal(wait_id)
    })
    .suspend();
    assert_eq!(
        result,
        WaitResult::Woken,
        "sigsuspend has no timeout/cancellation path"
    );
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
        // Release 发布 local Processor 初始化；缺失时远端选核可能向尚未开始 drain 的 hart 投递任务。
        hart::mark_active();
    });
    loop {
        // 1. 关中断覆盖 deferred work、mailbox drain 和 task select，保证 idle 决策看到一致状态。
        let idle_irq = LocalIrqGuard::disable();
        dispatch_pending_deferred_work();
        with_current_processor(|processor| processor.drain_inbound_to_local());
        let task = with_current_processor(Processor::select_task);
        if let Some(task) = task {
            switch_to_task(task);
            // guard 留在 idle stack 跨越切换，确保 kernel continuation 恢复时 SIE=0。
            drop(idle_irq);
            continue;
        }

        // 2. SIE=0 时执行 WFI，已在 sie 中单独使能的 timer/IPI 仍会让 hart 退出等待。
        // 3. 醒来后才短暂打开 SIE 投递 pending trap；若在 WFI 前开中断，trap 可能先于
        //    WFI 返回并造成 lost wakeup。
        // idle context 不继承 user trap 的 SIE=0：醒来后显式投递一次 pending interrupt。
        use riscv::asm::wfi;
        wfi();
        drop(idle_irq);
        // SAFETY: 当前 hart 没有 running task，所有 scheduler guard 已释放；只为投递本地 pending trap 修改 SIE。
        unsafe { riscv::register::sstatus::set_sie() }
        // SAFETY: 与上面的 idle-only enable 配对，恢复 kernel continuation 的 non-nested 契约。
        unsafe { riscv::register::sstatus::clear_sie() }
    }
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

/// @description 将当前 task 加入 indexed wait registry 并切回 idle。
///
/// @param deadline 绝对 monotonic 纳秒 deadline。
/// @return task 被重新调度后返回 timeout 或 signal interruption 结果。
fn block_current_until(deadline: u64) -> WaitResult {
    let task = current_task().expect("deadline wait requires current task");
    // 1. wait owner lock 将 signal 复查与 membership 发布串行化，封闭 lost wakeup。
    let queue = INDEXED_WAIT_QUEUE.lock();
    if task.has_deliverable_signal() {
        return WaitResult::Interrupted;
    }
    // 2. deep blocking seam 同时提交 runtime、发布 membership 并在切换前释放 registry owner。
    prepare_current_block(&task, queue, |queue, current| {
        let wait_id = queue.insert_deadline(deadline, current);
        WaitMembership::Deadline(wait_id)
    })
    .suspend()
}

/// 切换到指定任务
fn switch_to_task(task: Arc<TaskControlBlock>) {
    let cpu = hart_id();
    with_current_processor(|processor| {
        let current = processor
            .current
            .as_ref()
            .expect("selected task missing from current");
        assert!(
            Arc::ptr_eq(current, &task),
            "selected task differs from current"
        );
    });
    assert_eq!(
        task.scheduling.state.lock().run_state,
        RunState::Running { cpu },
        "selected task must be Running on this CPU"
    );

    let start_time = get_time_us();
    task.scheduling.policy.lock().begin_runtime(start_time);
    // last_cpu 只记录下次调度 hint，不发布 task 内部状态。
    task.scheduling.last_cpu.store(cpu, Ordering::Relaxed);

    // 只保留 raw context 地址，避免 mutable Processor borrow 跨越切换。
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);

    // 获取任务上下文地址
    let next_task_cx_ptr = {
        let task_cx = task.task_context().lock();
        &*task_cx as *const TaskContext
    };

    // 验证指针有效性
    if next_task_cx_ptr.is_null() {
        panic!("Invalid task context pointer");
    }

    // 所有 guard 已释放，只携带由 task Arc 保活的 context raw pointer。
    // SAFETY: owning task Arc keeps the next context alive, the hart-local idle context is an
    // exclusive save target, and scheduling state prevents concurrent execution of next.
    unsafe {
        crate::task::__switch(idle_task_cx_ptr, next_task_cx_ptr);
    }
    if crate::task::processor::finish_deschedule_transition(&task) {
        complete_process_stop(task.tgid());
    }
    // 退出 task 把自身 Arc 留在 per-hart slot；这里只在已经恢复的 idle stack 上析构。
    crate::task::processor::reap_deferred_task();
}
