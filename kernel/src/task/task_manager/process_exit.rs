use super::*;
use alloc::sync::Arc;

const EXIT_EFFECT_HANGUP: u8 = 1 << 0;
const EXIT_EFFECT_CONTINUE: u8 = 1 << 1;

fn group_is_stopped(graph: &ProcessGraph, group: (usize, usize)) -> bool {
    graph.groups.get(&group).is_some_and(|group| {
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
        .get(&group)
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
        .get_mut(&group)
        .expect("staged process group disappeared");
    index.exit_check_pending = true;
    index.exit_check_next = next;
    index.was_orphaned_stopped = was_orphaned_stopped;
    graph.exit_group_head = Some(group);
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
        let (next, was_orphaned_stopped) = {
            let index = graph
                .groups
                .get_mut(&group)
                .expect("queued process group disappeared");
            index.exit_check_pending = false;
            (
                index.exit_check_next.take(),
                core::mem::take(&mut index.was_orphaned_stopped),
            )
        };
        graph.exit_group_head = next;
        let is_orphaned_stopped =
            super::process_group::process_group_is_orphaned(graph, group.0, group.1)
                && group_is_stopped(graph, group);
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
            let mut graph = TASK_MANAGER.graph.lock();
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
    let (kernel_context, idle_context) = prepare_current_exit(requested);
    // SAFETY: prepare_current_exit 已把退出 task 的唯一调度 owner 移交 deferred-reap slot；
    // 两个 context 都由该 CPU 独占，且本 frame 不保留任何指向退出 task 的 Arc。
    unsafe { crate::arch::context::switch_kernel_context(kernel_context, idle_context) };
    panic!("exited task context resumed")
}

/// @description 完成退出副作用，并在仍可正常展开 Rust frame 时释放所有 task Arc。
///
/// @param requested calling Thread 请求的退出原因。
/// @return 依次为 task/idle raw context 地址；task 由 deferred-reap slot 保活。
fn prepare_current_exit(requested: ProcessExitStatus) -> (*mut KernelContext, *mut KernelContext) {
    // Memory/file cleanup can contend on task-context mutexes. Keep the exiting task installed as
    // current until every blocking consequence completes; removing it earlier leaves no scheduler
    // wait target and turns ordinary same-mm contention into a kernel panic.
    let task = current_task().expect("No current task to exit");
    task.cleanup_robust_list();
    let (removed, process_status, parent_waiters, init_waiters, parent_signal_pid) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let exiting_pid = task.tgid();
        let process_will_exit = graph.nodes.get(&exiting_pid).is_some_and(
            |node| matches!(&node.state, ProcessState::Live(threads) if threads.len() == 1),
        );
        if process_will_exit {
            // Affected groups are the exiting process group plus its direct children's groups.
            // The owner index freezes their old orphan/stopped state without a graph snapshot.
            mark_orphaned_stopped_groups(&mut graph, exiting_pid);
        }
        let (removed, process_status, parent, session_leader, replacement_parent_tid) = {
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
            if task.parent_death_signal(None) != 0 {
                node.pdeath_enabled_threads -= 1;
            }
            let process_status = threads
                .is_empty()
                .then(|| node.group_exit.take().unwrap_or(requested));
            let replacement_parent_tid =
                threads.first_key_value().map_or(INIT_PID, |(&tid, _)| tid);
            let parent = node.parent;
            let session_leader = node.session == exiting_pid;
            if let Some(status) = process_status {
                assert!(node.child_waiters.is_empty());
                node.state = ProcessState::Exited(status);
            }
            (
                removed,
                process_status,
                parent,
                session_leader,
                replacement_parent_tid,
            )
        };
        assert!(Arc::ptr_eq(&removed, &task));
        super::parent_death::mark_parent_exit(
            &mut graph,
            exiting_pid,
            task.tid(),
            replacement_parent_tid,
        );
        if process_status.is_some() {
            // graph → timer 与 set/get 共用唯一锁序；持 graph 期间删除使 exit 后不存在 stale timer。
            TASK_MANAGER.timers.lock().remove_process(exiting_pid);
        }

        match process_status {
            None => (removed, None, FallibleMap::new(), FallibleMap::new(), None),
            Some(status) => {
                // orphan membership nodes move to init in the same owner transaction. No
                // allocation can fail after the first edge has moved.
                let mut adopted_exited = false;
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
                        node.parent = Some(INIT_PID);
                        adopted_exited |= matches!(node.state, ProcessState::Exited(_));
                        graph
                            .nodes
                            .get_mut(&INIT_PID)
                            .expect("init missing during orphan reparent")
                            .children
                            .commit_vacant(membership);
                    }
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
                    let group = (exiting_pid, foreground);
                    let mut cursor = 0;
                    loop {
                        let member = graph
                            .groups
                            .get(&group)
                            .and_then(|group| group.members.successor(&cursor))
                            .map(|(&tgid, ())| tgid);
                        let Some(tgid) = member else {
                            break;
                        };
                        cursor = tgid;
                        stage_exit_effect(&mut graph, tgid, EXIT_EFFECT_HANGUP);
                    }
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

    // pdeath generation 已在 graph mutation 内冻结；锁外先投递，保证 creator exit
    // consequence 不晚于 orphan/session 与 SIGCHLD observer。
    super::parent_death::drain_parent_death_signals();

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
    // vfork child exit 只有在 RISC-V 临时 trap VMA 删除（AArch64 kernel-stack backing retire）
    // 后才能恢复 parent；否则 parent 可与仍持有 shared-mm mapping 的 child cleanup 并发。
    complete_vfork(task.tgid());
    // 两个来源分别 staged，按 parent 后 init 的既有来源优先级各 drain 一次；waiter
    // identity 不依赖跨来源 TID 排序，合并反而会制造没有领域意义的 AVL interface。
    drain_staged_child_waiters(parent_waiters);
    drain_staged_child_waiters(init_waiters);
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
