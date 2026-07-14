use super::*;
use alloc::sync::Arc;

const EXIT_EFFECT_HANGUP: u8 = 1 << 0;
const EXIT_EFFECT_CONTINUE: u8 = 1 << 1;
const EXIT_EFFECT_WAS_ORPHANED_STOPPED: u8 = 1 << 2;

fn mark_orphaned_stopped_groups(graph: &mut ProcessGraph, effect: u8) {
    let mut cursor = 0;
    loop {
        let candidate = graph.nodes.iter_after(&cursor).find_map(|(&tgid, node)| {
            (matches!(node.state, ProcessState::Live(_))
                && node.job_control == JobControlState::Stopped)
                .then_some((tgid, node.session, node.process_group))
        });
        let Some((tgid, session, process_group)) = candidate else {
            break;
        };
        cursor = tgid;
        if !super::process_group::process_group_is_orphaned(graph, session, process_group) {
            continue;
        }
        graph.nodes.for_each_mut(|_, node| {
            if node.session == session
                && node.process_group == process_group
                && matches!(node.state, ProcessState::Live(_))
            {
                node.exit_effects |= effect;
            }
        });
    }
}

fn mark_new_orphaned_stopped_groups(graph: &mut ProcessGraph) {
    let mut cursor = 0;
    loop {
        let candidate = graph.nodes.iter_after(&cursor).find_map(|(&tgid, node)| {
            (matches!(node.state, ProcessState::Live(_))
                && node.job_control == JobControlState::Stopped)
                .then_some((tgid, node.session, node.process_group))
        });
        let Some((tgid, session, process_group)) = candidate else {
            break;
        };
        cursor = tgid;
        if !super::process_group::process_group_is_orphaned(graph, session, process_group)
            || graph.nodes.values().any(|node| {
                node.session == session
                    && node.process_group == process_group
                    && node.exit_effects & EXIT_EFFECT_WAS_ORPHANED_STOPPED != 0
            })
        {
            continue;
        }
        graph.nodes.for_each_mut(|_, node| {
            if node.session == session
                && node.process_group == process_group
                && matches!(node.state, ProcessState::Live(_))
            {
                node.exit_effects |= EXIT_EFFECT_HANGUP | EXIT_EFFECT_CONTINUE;
            }
        });
    }
    graph.nodes.for_each_mut(|_, node| {
        node.exit_effects &= !EXIT_EFFECT_WAS_ORPHANED_STOPPED;
    });
}

fn drain_exit_effect(effect: u8, signal: usize) {
    loop {
        let target = {
            let mut graph = TASK_MANAGER.graph.lock();
            let target = graph
                .nodes
                .iter()
                .find_map(|(&tgid, node)| (node.exit_effects & effect != 0).then_some(tgid));
            if let Some(tgid) = target {
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

/// @description 若当前 Process 已提交 group-exit，则在 calling Thread 上完成退出。
///
/// @return 普通运行态直接返回；group-exit 已提交时不返回。
pub(crate) fn exit_current_if_group_exiting() {
    let task = current_task().expect("group-exit query requires current task");
    let status = TASK_MANAGER
        .graph
        .lock()
        .nodes
        .get(&task.tgid())
        .and_then(|node| node.group_exit);
    drop(task);
    if let Some(status) = status {
        exit_current(status);
    }
}

/// @description 按 Linux `exit` 语义只终止 calling Thread。
///
/// @param code 用户提供的低 8-bit exit code。
/// @return 此函数不返回。
pub(crate) fn exit_current_thread(code: i32) -> ! {
    exit_current(ProcessExitStatus::Exited(code as u8))
}

/// @description 按 Linux `exit_group` 语义提交正常退出原因并终止整个 Thread Group。
///
/// @param code 用户提供的低 8-bit process exit code。
/// @return 此函数不返回。
pub(crate) fn exit_current_group(code: i32) -> ! {
    let status = begin_group_exit(ProcessExitStatus::Exited(code as u8));
    exit_current(status)
}

/// @description 提交默认致命 signal 原因并终止整个 Thread Group。
///
/// @param signal Linux signal number，必须可由 wait status 低 7-bit 表达。
/// @return 此函数不返回。
pub(crate) fn exit_current_group_by_signal(signal: usize) -> ! {
    assert!((1..=64).contains(&signal), "invalid fatal signal");
    let status = begin_group_exit(ProcessExitStatus::Signaled(signal as u8));
    exit_current(status)
}

fn begin_group_exit(requested: ProcessExitStatus) -> ProcessExitStatus {
    let current = current_task().expect("group exit requires current task");
    let current_tid = current.tid();
    let (status, initiated) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let node = graph
            .nodes
            .get_mut(&current.tgid())
            .expect("group exit process missing from graph");
        let ProcessState::Live(threads) = &node.state else {
            panic!("exited process began group exit");
        };
        if let Some(status) = node.group_exit {
            (status, false)
        } else {
            // 首个发起者唯一决定 parent-visible status；缺少该 owner 会让并发 fatal signal
            // 与 exit_group 互相覆盖，最终 wait4 结果取决于竞态。
            node.group_exit = Some(requested);
            node.job_control = JobControlState::Running;
            // SIGKILL 没有 stop/continue 冲突集，可在 graph owner 内直接逐 Thread
            // publication；因此无需为 exit 这条不可失败路径分配 Arc snapshot。
            for thread in threads
                .values()
                .filter(|thread| thread.tid() != current_tid)
            {
                thread
                    .queue_signal(core::iter::empty(), 9, PendingSignal::kernel())
                    .expect("kernel SIGKILL must be valid");
            }
            (requested, true)
        }
    };

    if !initiated {
        return status;
    }

    // 1. group_exit 禁止新增 sibling；按 TID cursor 每次只 clone 一个 Arc，锁外进入
    // scheduler/wait seam，既不分配 snapshot，也不持 graph lock 反转锁序。
    // 2. 并发已退出 sibling 会从下一次 graph lookup 自然消失。
    let mut cursor = 0;
    loop {
        let next = {
            let graph = TASK_MANAGER.graph.lock();
            graph
                .nodes
                .get(&current.tgid())
                .and_then(|node| match &node.state {
                    ProcessState::Live(threads) => threads
                        .iter_after(&cursor)
                        .find(|(tid, _)| **tid != current_tid)
                        .map(|(&tid, thread)| (tid, thread.clone())),
                    ProcessState::Exited(_) => None,
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
    status
}

fn exit_current(requested: ProcessExitStatus) -> ! {
    let (task_context, idle_context) = prepare_current_exit(requested);
    // SAFETY: prepare_current_exit 已把退出 task 的唯一调度 owner 移交 deferred-reap slot；
    // 两个 context 都由该 hart 独占，且本 frame 不保留任何指向退出 task 的 Arc。
    unsafe { crate::task::__switch(task_context, idle_context) };
    panic!("exited task context resumed")
}

/// @description 完成退出副作用，并在仍可正常展开 Rust frame 时释放所有 task Arc。
///
/// @param requested calling Thread 请求的退出原因。
/// @return 依次为 task/idle raw context 地址；task 由 deferred-reap slot 保活。
fn prepare_current_exit(requested: ProcessExitStatus) -> (*mut TaskContext, *mut TaskContext) {
    let task = take_current_task().expect("No current task to exit");
    let end_time = get_time_us();
    task.scheduling.policy.lock().finish_runtime(end_time);

    {
        let mut scheduling = task.scheduling.state.lock();
        assert!(
            matches!(scheduling.run_state, RunState::Running { .. }),
            "only current running task can exit"
        );
        assert!(
            scheduling.wait.is_none(),
            "running task cannot retain wait membership"
        );
        scheduling.run_state = RunState::Exited;
    }
    task.cleanup_robust_list();
    let (removed, process_status, parent_waiters, init_waiters, parent_signal_pid) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let exiting_pid = task.tgid();
        let process_will_exit = graph.nodes.get(&exiting_pid).is_some_and(
            |node| matches!(&node.state, ProcessState::Live(threads) if threads.len() == 1),
        );
        if process_will_exit {
            // 临时 bit 与 graph mutation 同锁：它冻结“退出前已 orphaned+stopped”的精确
            // membership，避免不可失败的 exit 路径分配 group/member snapshot。
            mark_orphaned_stopped_groups(&mut graph, EXIT_EFFECT_WAS_ORPHANED_STOPPED);
        }
        let (removed, process_status, parent, session_leader) = {
            let node = graph
                .nodes
                .get_mut(&exiting_pid)
                .expect("exiting task missing from process graph");
            let ProcessState::Live(threads) = &mut node.state else {
                panic!("process exited twice");
            };
            let removed = threads
                .remove(&task.tid())
                .expect("exiting thread missing from process graph");
            let process_status = threads
                .is_empty()
                .then(|| node.group_exit.take().unwrap_or(requested));
            let parent = node.parent;
            let session_leader = node.session == exiting_pid;
            if let Some(status) = process_status {
                assert!(node.child_waiters.is_empty());
                node.state = ProcessState::Exited(status);
            }
            (removed, process_status, parent, session_leader)
        };
        assert!(Arc::ptr_eq(&removed, &task));
        if process_status.is_some() {
            // graph → timer 与 set/get 共用唯一锁序；持 graph 期间删除使 exit 后不存在 stale timer。
            TASK_MANAGER.real_timers.lock().remove(exiting_pid);
        }

        match process_status {
            None => (removed, None, FallibleMap::new(), FallibleMap::new(), None),
            Some(status) => {
                // 1. orphan 只改写 graph 中的唯一 parent edge；不复制 child collection。
                if exiting_pid != INIT_PID {
                    graph.nodes.for_each_mut(|_, child| {
                        if child.parent == Some(exiting_pid) {
                            child.parent = Some(INIT_PID);
                        }
                    });
                }
                // 2. 取走 waiter owner 后释放 graph lock，再进入 scheduler seam，避免锁序反转。
                let parent_waiters = parent
                    .and_then(|pid| graph.nodes.get_mut(&pid))
                    .map(take_child_waiters)
                    .unwrap_or_default();
                let parent_signal_pid = parent.filter(|pid| {
                    graph
                        .nodes
                        .get(pid)
                        .is_some_and(|node| matches!(node.state, ProcessState::Live(_)))
                });
                let adopted_exited = exiting_pid != INIT_PID
                    && graph.nodes.values().any(|child| {
                        child.parent == Some(INIT_PID)
                            && matches!(child.state, ProcessState::Exited(_))
                    });
                let init_waiters = if adopted_exited {
                    graph
                        .nodes
                        .get_mut(&INIT_PID)
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
                    && let Some(foreground) = task.terminal().release_session(exiting_pid)
                {
                    graph.nodes.for_each_mut(|_, node| {
                        if node.session == exiting_pid
                            && node.process_group == foreground
                            && matches!(node.state, ProcessState::Live(_))
                        {
                            node.exit_effects |= EXIT_EFFECT_HANGUP;
                        }
                    });
                }
                mark_new_orphaned_stopped_groups(&mut graph);
                (
                    removed,
                    Some(status),
                    parent_waiters,
                    init_waiters,
                    parent_signal_pid,
                )
            }
        }
    };

    // 退出导致的 terminal/orphan signal 必须先于 parent wake/SIGCHLD；否则 parent 可先
    // reap 并推进 shell 状态，使 POSIX exit consequences 的观察顺序依赖调度竞态。
    drain_exit_effects();

    // 1. process graph 先注销 Thread owner，再发布 clear-child-tid completion。
    // 2. 若顺序相反，pthread_join 可在 graph 仍计数已退出 sibling 时返回，使紧随的
    //    single-thread-only fork/exec 错误观察到 EAGAIN。
    if let Some(address) = task.take_clear_child_tid()
        && task.copy_to_user(address, &0u32.to_ne_bytes()).is_ok()
    {
        let _ = futex_wake(&task, address, false, 1, u32::MAX);
    }
    if process_status.is_some() {
        task.close_all_files();
    }
    drop(removed);
    task.remove_thread_trap_context();
    // vfork child exit 只有在临时 trap page 已从共享 AddressSpace 删除后才能恢复 parent；
    // 否则 parent 可与仍持有 shared-mm supervisor mapping 的 child cleanup 并发。
    complete_vfork(task.tgid());
    let mut waiters = parent_waiters;
    let mut init_waiters = init_waiters;
    waiters.append(&mut init_waiters);
    while let Some((&tid, _)) = waiters.first_key_value() {
        let waiter = waiters.remove(&tid).expect("staged child waiter");
        crate::task::processor::wake_child_task(waiter, WaitResult::Woken);
    }
    if let (Some(parent), Some(status)) = (parent_signal_pid, process_status) {
        let info = match status {
            ProcessExitStatus::Exited(code) => {
                PendingSignal::child_exited(task.tgid(), i32::from(code))
            }
            ProcessExitStatus::Signaled(signal) => {
                PendingSignal::child_killed(task.tgid(), usize::from(signal))
            }
        };
        send_kernel_process_signal(parent, 17, info);
    }
    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let task_cx_ptr = {
        let mut task_cx = task.task_context().lock();
        &mut *task_cx as *mut TaskContext
    };

    crate::task::processor::defer_task_reap(task);
    (task_cx_ptr, idle_task_cx_ptr)
}
