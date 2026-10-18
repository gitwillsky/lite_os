use super::*;
use alloc::sync::Arc;

const EXIT_EFFECT_HANGUP: u8 = 1 << 0;
const EXIT_EFFECT_CONTINUE: u8 = 1 << 1;

fn group_is_stopped(graph: &ProcessGraph, group: (usize, usize)) -> bool {
    graph.groups.get(&group.1).is_some_and(|group| {
        group.members.iter().any(|(&tgid, ())| {
            graph.nodes.get(&tgid).is_some_and(|node| {
                matches!(node.state, ProcessState::Live(_))
                    && node.job_control == JobControlState::Stopped
            })
        })
    })
}

fn stage_group_exit_check(graph: &mut ProcessGraph, group: (usize, usize)) {
    if graph
        .groups
        .get(&group.1)
        .is_none_or(|index| index.exit_check_pending)
    {
        return;
    }
    let was_orphaned_stopped =
        super::process_group::process_group_is_orphaned(graph, group.0, group.1)
            && group_is_stopped(graph, group);
    let next = graph.exit_group_head;
    let index = graph
        .groups
        .get_mut(&group.1)
        .expect("staged process group disappeared");
    index.exit_check_pending = true;
    index.exit_check_next = next;
    index.was_orphaned_stopped = was_orphaned_stopped;
    graph.exit_group_head = Some(group.1);
}

fn mark_orphaned_stopped_groups(graph: &mut ProcessGraph, exiting: usize) {
    let Some(node) = graph.nodes.get(&exiting) else {
        return;
    };
    stage_group_exit_check(graph, (node.session, node.process_group));
    let mut cursor = 0;
    loop {
        let child = graph
            .nodes
            .get(&exiting)
            .and_then(|node| node.children.successor(&cursor))
            .map(|(&child, ())| child);
        let Some(child) = child else {
            break;
        };
        cursor = child;
        let node = graph
            .nodes
            .get(&child)
            .expect("child index references missing process");
        stage_group_exit_check(graph, (node.session, node.process_group));
    }
}

fn stage_exit_effect(graph: &mut ProcessGraph, tgid: usize, effect: u8) {
    let index = effect.trailing_zeros() as usize;
    let head = graph.exit_effect_heads[index];
    let node = graph
        .nodes
        .get_mut(&tgid)
        .expect("exit-effect process missing from graph");
    if node.exit_effects & effect != 0 || !matches!(node.state, ProcessState::Live(_)) {
        return;
    }
    node.exit_effects |= effect;
    node.exit_effect_next[index] = head;
    graph.exit_effect_heads[index] = Some(tgid);
}

fn mark_new_orphaned_stopped_groups(graph: &mut ProcessGraph) {
    while let Some(group) = graph.exit_group_head {
        let (next, was_orphaned_stopped, session) = {
            let index = graph
                .groups
                .get_mut(&group)
                .expect("queued process group disappeared");
            index.exit_check_pending = false;
            (
                index.exit_check_next.take(),
                core::mem::take(&mut index.was_orphaned_stopped),
                index.sid.identity().key(),
            )
        };
        graph.exit_group_head = next;
        let is_orphaned_stopped =
            super::process_group::process_group_is_orphaned(graph, session, group)
                && group_is_stopped(graph, (session, group));
        if was_orphaned_stopped || !is_orphaned_stopped {
            continue;
        }
        let mut cursor = 0;
        loop {
            let member = graph
                .groups
                .get(&group)
                .and_then(|index| index.members.successor(&cursor))
                .map(|(&tgid, ())| tgid);
            let Some(tgid) = member else {
                break;
            };
            cursor = tgid;
            stage_exit_effect(graph, tgid, EXIT_EFFECT_HANGUP);
            stage_exit_effect(graph, tgid, EXIT_EFFECT_CONTINUE);
        }
    }
}

fn drain_exit_effect(effect: u8, signal: usize) {
    loop {
        let target = {
            let mut graph = PROCESS_TABLE.graph.lock();
            let index = effect.trailing_zeros() as usize;
            let target = graph.exit_effect_heads[index];
            if let Some(tgid) = target {
                let next = graph
                    .nodes
                    .get_mut(&tgid)
                    .expect("selected exit-effect process disappeared")
                    .exit_effect_next[index]
                    .take();
                graph.exit_effect_heads[index] = next;
                graph
                    .nodes
                    .get_mut(&tgid)
                    .expect("selected exit-effect process disappeared")
                    .exit_effects &= !effect;
            }
            target
        };
        let Some(tgid) = target else {
            break;
        };
        send_kernel_process_signal(tgid, signal, PendingSignal::kernel());
    }
}

fn drain_exit_effects() {
    // POSIX orphan handling requires SIGHUP generation before SIGCONT. Two global phases preserve
    // that order without allocating an unbounded group/member snapshot in the exit path.
    drain_exit_effect(EXIT_EFFECT_HANGUP, 1);
    drain_exit_effect(EXIT_EFFECT_CONTINUE, 18);
}

fn drain_staged_child_waiters(mut waiters: FallibleMap<usize, Arc<TaskControlBlock>>) {
    while let Some((&tid, _)) = waiters.first_key_value() {
        let waiter = waiters.remove(&tid).expect("staged child waiter");
        crate::task::processor::wake_child_task(waiter, WaitResult::Woken);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessExitStatus {
    Exited(u8),
    Signaled(u8),
}

impl ProcessExitStatus {
    pub(super) fn wait_status(self) -> i32 {
        match self {
            Self::Exited(code) => i32::from(code) << 8,
            Self::Signaled(signal) => i32::from(signal) & 0x7f,
        }
    }
}

fn child_exit_info(
    pid: usize,
    status: ProcessExitStatus,
    usage: &crate::task::ResourceUsage,
) -> PendingSignal {
    // si_utime/si_stime 是 USER_HZ（100）的 clock_t，包含 child 已回收的后代。
    const MICROSECONDS_PER_CLOCK_TICK: u64 = 10_000;
    match status {
        ProcessExitStatus::Exited(code) => PendingSignal::child_exited(pid, i32::from(code)),
        ProcessExitStatus::Signaled(signal) => {
            PendingSignal::child_killed(pid, usize::from(signal))
        }
    }
    .with_child_cpu_times(
        usage.user_us / MICROSECONDS_PER_CLOCK_TICK,
        usage.system_us / MICROSECONDS_PER_CLOCK_TICK,
    )
}

/// 若当前 Process 已提交 group-exit，则在 calling Thread 上完成退出。
///
/// # Returns
///
/// 普通运行态直接返回；group-exit 已提交时不返回。
pub(crate) fn exit_current_if_group_exiting() {
    let task = current_task().expect("group-exit query requires current task");
    let status = PROCESS_TABLE
        .graph
        .lock()
        .nodes
        .get(&task.tgid())
        .and_then(|node| {
            node.group_exit.or_else(|| {
                node.exec
                    .as_ref()
                    .filter(|exec| exec.kills(task.tid()))
                    .map(|_| ProcessExitStatus::Signaled(syscall_abi::signal::SIGKILL as u8))
            })
        });
    drop(task);
    if let Some(status) = status {
        exit_current(status);
    }
}

/// 按 Linux `exit` 语义只终止 calling Thread。
///
/// # Parameters
///
/// - `code`: 用户提供的低 8-bit exit code。
///
/// # Returns
///
/// 此函数不返回。
pub(crate) fn exit_current_thread(code: i32) -> ! {
    exit_current(ProcessExitStatus::Exited(code as u8))
}

/// seccomp 的 thread-only fatal consequence；最后一个 Thread 的 wait status 保留 signal。
///
/// `signal` 必须在 1..=64；不返回，不向 sibling 发布 group-exit 或 SIGKILL。
pub(crate) fn exit_current_thread_by_signal(signal: usize) -> ! {
    assert!((1..=64).contains(&signal), "invalid fatal signal");
    exit_current(ProcessExitStatus::Signaled(signal as u8))
}

/// 按 Linux `exit_group` 语义提交正常退出原因并终止整个 Thread Group。
///
/// # Parameters
///
/// - `code`: 用户提供的低 8-bit process exit code。
///
/// # Returns
///
/// 此函数不返回。
pub(crate) fn exit_current_group(code: i32) -> ! {
    let status = begin_group_exit(ProcessExitStatus::Exited(code as u8), false);
    exit_current(status)
}

/// 提交默认致命 signal 原因并终止整个 Thread Group。
///
/// # Parameters
///
/// - `signal`: Linux signal number，必须可由 wait status 低 7-bit 表达。
///
/// # Returns
///
/// 此函数不返回。
pub(crate) fn exit_current_group_by_signal(signal: usize) -> ! {
    assert!((1..=64).contains(&signal), "invalid fatal signal");
    let status = begin_group_exit(ProcessExitStatus::Signaled(signal as u8), false);
    exit_current(status)
}

/// exec 越过 PONR 后的唯一失败 consequence；已有 SIGKILL 或已提交 group status 优先。
/// syscall 必须先退休新映像/参数/TCB Arc，再调用此不返回入口。
pub(crate) fn exit_current_after_exec_failure() -> ! {
    let status = begin_group_exit(
        ProcessExitStatus::Signaled(syscall_abi::signal::SIGSEGV as u8),
        true,
    );
    exit_current(status)
}

fn begin_group_exit(
    requested: ProcessExitStatus,
    preserve_pending_fatal: bool,
) -> ProcessExitStatus {
    let current = current_task().expect("group exit requires current task");
    request_group_exit(&current, requested, preserve_pending_fatal, false)
        .expect("current exiting process remains live")
}

pub(super) fn request_group_exit(
    current: &TaskControlBlock,
    requested: ProcessExitStatus,
    preserve_pending_fatal: bool,
    include_initiator: bool,
) -> Option<ProcessExitStatus> {
    let current_tid = current.tid();
    let (status, initiated) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let node = graph.nodes.get_mut(&current.tgid())?;
        let ProcessState::Live(threads) = &node.state else {
            return None;
        };
        if let Some(status) = node.group_exit {
            (status, false)
        } else {
            // 与 signal publication 共用 graph 边界；PONR OOM 不覆盖先到达的 SIGKILL。
            let requested = if preserve_pending_fatal && current.has_fatal_signal() {
                ProcessExitStatus::Signaled(syscall_abi::signal::SIGKILL as u8)
            } else {
                requested
            };
            // 首个发起者唯一决定 parent-visible status；缺少该 owner 会让并发 fatal signal
            // 与 exit_group 互相覆盖，最终 wait4 结果取决于竞态。
            node.group_exit = Some(requested);
            node.job_control = JobControlState::Running;
            // SIGKILL 没有 stop/continue 冲突集，可在 graph owner 内直接逐 Thread
            // publication；因此无需为 exit 这条不可失败路径分配 Arc snapshot。
            for thread in threads
                .values()
                .filter(|thread| include_initiator || thread.tid() != current_tid)
            {
                thread.queue_group_exit_signal();
            }
            (requested, true)
        }
    };

    if !initiated {
        return Some(status);
    }

    // 1. group_exit 禁止新增 sibling；按 TID cursor 每次只 clone 一个 Arc，锁外进入
    // scheduler/wait seam，既不分配 snapshot，也不持 graph lock 反转锁序。
    // 2. 并发已退出 sibling 会从下一次 graph lookup 自然消失。
    let mut cursor = 0;
    loop {
        let next = {
            let graph = PROCESS_TABLE.graph.lock();
            graph
                .nodes
                .get(&current.tgid())
                .and_then(|node| match &node.state {
                    ProcessState::Live(threads) => threads
                        .iter_after(&cursor)
                        .find(|(tid, _)| include_initiator || **tid != current_tid)
                        .map(|(&tid, thread)| (tid, thread.clone())),
                    ProcessState::Exited(..) => None,
                })
        };
        let Some((tid, thread)) = next else {
            break;
        };
        cursor = tid;
        crate::task::processor::continue_stopped_task(thread.clone());
        super::signal::interrupt_waiting_task(&thread);
        crate::task::processor::request_task_reschedule(&thread);
    }
    Some(status)
}

/// 终止主体已返回的当前内核线程。
///
/// 内核线程没有用户 Process、exit status 或 parent：撤销实际 kernel execution membership 与调度 ownership、交给
/// deferred reap 并切到 idle。主体持有的资源已随其返回释放。
pub(in crate::task) fn exit_current_kernel_thread() -> ! {
    // 主体返回已完成所有 mm/IPC 退休；实际 PID membership 不得跟随 deferred TCB observer。
    let current = current_task().expect("exiting kernel thread lost current ownership");
    let source = current.kernel_pidfd_source(None).0;
    let retired = {
        let mut graph = PROCESS_TABLE.graph.lock();
        if let Some(source) = &source {
            source.record_exit(0);
        }
        graph
            .kernel_threads
            .take_entry(&current.tid())
            .expect("exiting kernel thread lost execution membership")
    };
    current.retire_kernel_pid();
    if let Some(source) = source {
        source.notify();
    }
    // 历史 observer 仍能 pin TCB；实际 graph/namespace admission 已退休，node/TCB Drop 不持 graph。
    drop(retired);
    drop(current);
    // 内核线程运行时开着调度中断；撤销 current 后、切换前被抢占会让 scheduler 观察到无 owner 的
    // Running task。退出 context 永不恢复，因此中断状态无需还原。
    let _masked = crate::arch::interrupt::disable_local();
    let (task_cx_ptr, idle_task_cx_ptr) = {
        let task = take_current_task().expect("exiting kernel thread lost current ownership");
        assert!(
            task.is_kernel_thread(),
            "user task took the kernel-thread exit path"
        );
        task.scheduling.policy.lock().finish_runtime(get_time_us());
        {
            let mut scheduling = task.scheduling.state.lock();
            assert!(
                matches!(scheduling.run_state(), RunState::Running { .. }),
                "only current running kernel thread can exit"
            );
            assert!(
                scheduling.wait.is_none(),
                "running kernel thread cannot retain wait membership"
            );
            scheduling.replace_non_ready_state(RunState::Exited);
        }
        let idle = with_current_processor(Processor::idle_context_ptr);
        let context = {
            let mut kernel_cx = task.kernel_context().lock();
            &mut *kernel_cx as *mut KernelContext
        };
        crate::task::processor::defer_task_reap(task);
        (context, idle)
    };
    // SAFETY: 调度 owner 已移交 deferred-reap slot；两个 context 由本 CPU 独占，本 frame 不再持有
    // 指向退出 task 的 Arc。
    unsafe { crate::arch::context::switch_kernel_context(task_cx_ptr, idle_task_cx_ptr) };
    panic!("exited kernel thread context resumed")
}

fn exit_current(requested: ProcessExitStatus) -> ! {
    let (kernel_context, idle_context) = prepare_current_exit(requested);
    // SAFETY: prepare_current_exit 已把退出 task 的唯一调度 owner 移交 deferred-reap slot；
    // 两个 context 都由该 CPU 独占，且本 frame 不保留任何指向退出 task 的 Arc。
    unsafe { crate::arch::context::switch_kernel_context(kernel_context, idle_context) };
    panic!("exited task context resumed")
}

/// 最后一个真实 Thread retirement 完成后的唯一 process-exit publication。
/// cleanup 期间 child 保持 Retiring；parent 的 wait/pending 与 autoreap 在这里同锁决定，
/// 避免父进程在 clear-child-tid/vfork completion 前 reap，或漏掉较早摘图但较晚清理的 sibling。
fn complete_process_exit(pid: usize) {
    let (waiters, wake, source, retired) = {
        let (mut records, mut graph) = super::signal::lock_child_notifications(
            pid,
            super::signal::ChildNotificationScope::Exit,
        )
        .expect("Retiring process must retain its exit owner");
        let node = graph.nodes.get_mut(&pid).expect("Retiring process missing");
        let ProcessState::Exited(status, ExitRetirement::Retiring) = node.state else {
            panic!("process exit published without actual retirement completion");
        };
        node.state = ProcessState::Exited(status, ExitRetirement::Waitable);
        let parent = node.parent;
        let source = node.pidfd_source.upgrade();
        let usage = node.process.final_resource_usage();
        let publication = super::signal::stage_child_signal(
            &graph,
            pid,
            child_exit_info(pid, status, &usage),
            super::super::model::ChildSignalKind::Exit,
            &mut records,
        );
        let wake = parent
            .filter(|_| {
                publication
                    .as_ref()
                    .is_some_and(|publication| publication.queued)
            })
            .map(|parent| {
                let signal = super::signal::exit_notification_signal(&graph, &graph.nodes[&pid]);
                super::signal::prepare_parent_signal_wake(&graph, parent, signal)
            });
        let waiters = parent
            .and_then(|parent| graph.nodes.get_mut(&parent))
            .map(take_child_waiters)
            .unwrap_or_default();
        let mut retired = super::process_retirement::RetiredProcesses::default();
        if publication.is_some_and(|publication| publication.autoreap) {
            let node = graph
                .nodes
                .get_mut(&pid)
                .expect("completed exit owner missing");
            if node.child_wait_claim.is_some() {
                node.state = ProcessState::Exited(status, ExitRetirement::AfterClaim);
            } else {
                retired.retire(&mut graph, pid);
            }
        }
        (waiters, wake, source, retired)
    };
    drop(retired);
    if let Some(source) = source {
        source.notify();
    }
    drain_staged_child_waiters(waiters);
    if let Some(wake) = wake {
        wake.publish();
    }
}

/// 完成退出副作用，并在仍可正常展开 Rust frame 时释放所有 task Arc。
///
/// # Parameters
///
/// - `requested`: calling Thread 请求的退出原因。
///
/// # Returns
///
/// 依次为 task/idle raw context 地址；task 由 deferred-reap slot 保活。
fn prepare_current_exit(requested: ProcessExitStatus) -> (*mut KernelContext, *mut KernelContext) {
    // Memory/file cleanup can contend on task-context mutexes. Keep the exiting task installed as
    // current until every blocking consequence completes; removing it earlier leaves no scheduler
    // wait target and turns ordinary same-mm contention into a kernel panic.
    let task = current_task().expect("No current task to exit");
    // 提交当前时间片：SIGCHLD 的 si_utime/si_stime 与 wait4 的 rusage 在 process exit 发布时读取最终
    // runtime，缺失这一步会漏掉最后一个不足 tick 的时间片。
    task.scheduling
        .policy
        .lock()
        .checkpoint_runtime(crate::timer::get_time_us());
    let namespace = task.pid_namespace();
    if let Some(event) = super::pid_namespace::mark_reaper_exit(&task, &namespace) {
        super::pid_namespace::shutdown(&task, &namespace, &event);
    }
    let exit_memory = task.prepare_user_exit_memory();
    // graph removal 领取持续 lifecycle owner；先于 exec drain 的退出也必须完成清理。
    let thread_retirement;
    let thread_status;
    let mut retired_terminal_control = None;
    let mut retired_process_timers = None;
    let (removed, process_status, init_waiters, init_signal_wake, retired, vfork_completion) = {
        let exiting_pid = task.tgid();
        let (mut child_records, mut graph) = super::signal::lock_child_notifications(
            exiting_pid,
            super::signal::ChildNotificationScope::ExitAdoption,
        )
        .expect("exiting Process missing during SIGCHLD preparation");
        let mut retired = super::process_retirement::RetiredProcesses::default();
        let reaper = super::pid_namespace::nearest_reaper(&graph, exiting_pid);
        let vfork_completion = super::vfork::take_vfork_completion(&mut graph, task.tid());
        let process_will_exit = graph.nodes.get(&exiting_pid).is_some_and(
            |node| matches!(&node.state, ProcessState::Live(threads) if threads.len() == 1),
        );
        // 最后一个 init Thread 是全局 child reaper；Linux 在此 fail-stop，不能先摘除
        // TID 再把 creator-child edge 迁给已经不存在的 init。其他 init sibling 可正常退出。
        assert!(
            exiting_pid != INIT_PID || !process_will_exit,
            "Attempted to kill init"
        );
        if process_will_exit {
            // Affected groups are the exiting process group plus its direct children's groups.
            // The owner index freezes their old orphan/stopped state without a graph snapshot.
            mark_orphaned_stopped_groups(&mut graph, exiting_pid);
        }
        // 已领取 PF_EXITING 的 sibling 仍可能阻塞在 cleanup，不能继续接收 creator-child
        // relation；没有可运行 sibling 时由实际祖先 reaper 接管，而非先后退出的临时次序。
        let replacement_parent_tid = match &graph.nodes.get(&exiting_pid).unwrap().state {
            ProcessState::Live(threads) => threads
                .iter()
                .find_map(|(&tid, _)| {
                    (!graph
                        .threads
                        .get(&tid)
                        .expect("live Thread is indexed")
                        .exit_in_progress)
                        .then_some(tid)
                })
                .unwrap_or(reaper.1),
            _ => unreachable!(),
        };
        let (removed, process_status, session_leader, replacement_parent_tid) = {
            let node = graph
                .nodes
                .get_mut(&exiting_pid)
                .expect("exiting task missing from process graph");
            let ProcessState::Live(threads) = &mut node.state else {
                panic!("process exited twice");
            };
            thread_retirement = task.thread_retirements().retirement();
            let removed = threads
                .remove(&task.tid())
                .expect("exiting thread missing from process graph");
            if task.parent_death_signal(None) != 0 {
                node.pdeath_enabled_threads -= 1;
            }
            let process_status = threads
                .is_empty()
                .then(|| node.group_exit.take().unwrap_or(requested));
            thread_status = process_status.unwrap_or_else(|| node.group_exit.unwrap_or(requested));
            let session_leader = node.session == exiting_pid;
            if let Some(status) = process_status {
                assert!(node.child_waiters.is_empty());
                node.state = ProcessState::Exited(status, ExitRetirement::Retiring);
            }
            (
                removed,
                process_status,
                session_leader,
                replacement_parent_tid,
            )
        };
        assert!(Arc::ptr_eq(&removed, &task));
        let index = graph
            .threads
            .get_mut(&task.tid())
            .expect("exiting Thread remains indexed");
        assert!(index.retiring_task.replace(removed.clone()).is_none());
        super::parent_death::mark_parent_exit(&mut graph, &task, replacement_parent_tid);
        if process_status.is_some() {
            // graph → timer 与 set/get 共用唯一锁序；持 graph 期间删除使 exit 后不存在 stale timer。
            retired_process_timers = Some(PROCESS_TABLE.timers.lock().remove_process(exiting_pid));
        }

        match process_status {
            None => (
                removed,
                None,
                FallibleMap::new(),
                None,
                retired,
                vfork_completion,
            ),
            Some(status) => {
                // orphan membership nodes move to init in the same owner transaction. No
                // allocation can fail after the first edge has moved.
                let mut adopted_exited = false;
                let mut init_signal_wake = None;
                if exiting_pid != INIT_PID {
                    loop {
                        let membership = {
                            let exiting = graph
                                .nodes
                                .get_mut(&exiting_pid)
                                .expect("exiting process disappeared during reparent");
                            let Some((&child, ())) = exiting.children.first_key_value() else {
                                break;
                            };
                            (
                                child,
                                exiting.children.take_entry(&child).expect("indexed child"),
                            )
                        };
                        let (child, membership) = membership;
                        let node = graph
                            .nodes
                            .get_mut(&child)
                            .expect("child index references missing process");
                        debug_assert_eq!(node.parent, Some(exiting_pid));
                        node.parent = Some(reaper.0);
                        // real parent 换成另一个 process：native orphan 一律成为普通 SIGCHLD child。
                        node.exit_signal = syscall_abi::signal::SIGCHLD as u8;
                        let child_status = match node.state {
                            ProcessState::Exited(_, ExitRetirement::Retiring) => None,
                            ProcessState::Exited(status, _) => Some(status),
                            _ => None,
                        };
                        let exited = child_status.is_some();
                        let child_usage = node.process.final_resource_usage();
                        let unclaimed = node.child_wait_claim.is_none();
                        adopted_exited |= exited;
                        graph
                            .nodes
                            .get_mut(&reaper.0)
                            .expect("namespace reaper missing during orphan reparent")
                            .children
                            .commit_vacant(membership);
                        let init_publication = child_status.and_then(|status| {
                            super::signal::stage_child_signal(
                                &graph,
                                child,
                                child_exit_info(child, status, &child_usage),
                                super::super::model::ChildSignalKind::Exit,
                                &mut child_records,
                            )
                        });
                        if init_publication
                            .as_ref()
                            .is_some_and(|publication| publication.queued)
                            && init_signal_wake.is_none()
                        {
                            init_signal_wake = Some(super::signal::prepare_parent_signal_wake(
                                &graph,
                                reaper.0,
                                syscall_abi::signal::SIGCHLD,
                            ));
                        }
                        if init_publication.is_some_and(|publication| publication.autoreap) {
                            if unclaimed {
                                retired.retire(&mut graph, child);
                            } else {
                                let node =
                                    graph.nodes.get_mut(&child).expect("adopted child missing");
                                let ProcessState::Exited(status, _) = node.state else {
                                    unreachable!();
                                };
                                node.state =
                                    ProcessState::Exited(status, ExitRetirement::AfterClaim);
                            }
                        }
                    }
                }
                let init_waiters = if adopted_exited {
                    graph
                        .nodes
                        .get_mut(&reaper.0)
                        .map(take_child_waiters)
                        .unwrap_or_default()
                } else {
                    FallibleMap::new()
                };
                // 1. process graph 是 SID/PGID membership owner，因此在同一 graph 临界区
                //    取走 Terminal foreground PGID，并把精确 live target 冻结到 owner bit。
                // 2. 统一使用 graph -> Terminal 锁序；反向路径都会先释放 Terminal lock 再发 signal，
                //    否则 session exit 与 TIOCSPGRP 并发时可能形成锁环或错发给后加入成员。
                // 3. signal 在锁外发布，避免 generation 再次进入 process graph 造成自锁。
                if session_leader
                    && let Some(terminal) = task.terminal()
                    && let Some(control) = terminal.release_session(exiting_pid)
                {
                    super::process_group::clear_terminal_session(&graph, exiting_pid, &terminal);
                    let foreground = control.foreground_group();
                    retired_terminal_control = Some(control);
                    if let Some(foreground) = foreground {
                        let group = (exiting_pid, foreground);
                        let mut cursor = 0;
                        loop {
                            let member = graph
                                .groups
                                .get(&group.1)
                                .and_then(|group| group.members.successor(&cursor))
                                .map(|(&tgid, ())| tgid);
                            let Some(tgid) = member else {
                                break;
                            };
                            cursor = tgid;
                            stage_exit_effect(&mut graph, tgid, EXIT_EFFECT_HANGUP);
                        }
                    }
                }
                mark_new_orphaned_stopped_groups(&mut graph);
                (
                    removed,
                    Some(status),
                    init_waiters,
                    init_signal_wake,
                    retired,
                    vfork_completion,
                )
            }
        }
    };

    drop(retired_process_timers);
    task.retire_thread_signals();
    drop(retired_terminal_control);
    drop(retired);
    if process_status.is_some() {
        task.retire_signal_handlers();
    }

    // pdeath generation 已在 graph mutation 内冻结；锁外先投递，保证 creator exit
    // consequence 不晚于 orphan/session 与 SIGCHLD observer。
    super::parent_death::drain_parent_death_signals();

    // 退出导致的 terminal/orphan signal 必须先于 parent wake/SIGCHLD；否则 parent 可先
    // reap 并推进 shell 状态，使 POSIX exit consequences 的观察顺序依赖调度竞态。
    drain_exit_effects();

    // 1. process graph 先注销 Thread owner，再发布 clear-child-tid completion。
    // 2. 若顺序相反，pthread_join 可在 graph 仍计数已退出 sibling 时返回，使紧随的
    //    single-thread-only fork/exec 错误观察到 EAGAIN。
    exit_memory.complete();
    drop(removed);
    // vfork child exit 只有在 RISC-V 临时 trap VMA 删除（AArch64 kernel-stack backing retire）
    // 后才能恢复 parent；否则 parent 可与仍持有 shared-mm mapping 的 child cleanup 并发。
    if let Some(completion) = vfork_completion {
        completion.complete();
    }
    // live execution 已摘下且 clear-child-tid 已完成；实际 PID association 至此才退休。
    // 保留原 ThreadIndex 至此，使 pidfd 不在可阻塞的 MM/vfork cleanup 中提前报告 HUP。
    let (retired_thread_index, thread_pidfd_notification) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let source = graph
            .threads
            .get(&task.tid())
            .and_then(|index| index.pidfd_source.upgrade());
        let retired = graph
            .threads
            .take_entry(&task.tid())
            .expect("exiting Thread retains actual PID association until cleanup");
        (retired, source)
    };
    if let Some(source) = &thread_pidfd_notification {
        source.record_exit(thread_status.wait_status());
    }
    drop(retired_thread_index);
    let process_completed = thread_retirement.complete();
    if let Some(source) = thread_pidfd_notification {
        source.notify();
    }
    if process_completed {
        complete_process_exit(task.tgid());
    }
    // 已完成的 orphan 通知仍由实际 adoption transaction 领取，不重新从最终 parent 推断。
    drain_staged_child_waiters(init_waiters);
    if let Some(wake) = init_signal_wake {
        wake.publish();
    }
    let current = take_current_task().expect("exiting task lost current ownership");
    assert!(Arc::ptr_eq(&current, &task));
    drop(current);
    task.scheduling.policy.lock().finish_runtime(get_time_us());
    {
        let mut scheduling = task.scheduling.state.lock();
        assert!(
            matches!(scheduling.run_state(), RunState::Running { .. }),
            "only current running task can exit"
        );
        assert!(
            scheduling.wait.is_none(),
            "running task cannot retain wait membership"
        );
        scheduling.replace_non_ready_state(RunState::Exited);
    }
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let task_cx_ptr = {
        let mut kernel_cx = task.kernel_context().lock();
        &mut *kernel_cx as *mut KernelContext
    };

    crate::task::processor::defer_task_reap(task);
    (task_cx_ptr, idle_task_cx_ptr)
}
