use alloc::{sync::Arc, vec::Vec};

use super::*;

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
    let (status, threads) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let node = graph
            .nodes
            .get_mut(&current.tgid())
            .expect("group exit process missing from graph");
        let ProcessState::Live(threads) = &node.state else {
            panic!("exited process began group exit");
        };
        if let Some(status) = node.group_exit {
            (status, None)
        } else {
            // 首个发起者唯一决定 parent-visible status；缺少该 owner 会让并发 fatal signal
            // 与 exit_group 互相覆盖，最终 wait4 结果取决于竞态。
            node.group_exit = Some(requested);
            node.job_control = JobControlState::Running;
            (
                requested,
                Some(threads.values().cloned().collect::<Vec<_>>()),
            )
        }
    };

    let Some(threads) = threads else {
        return status;
    };
    // 1. 与 Linux zap_other_threads 相同，用不可屏蔽 signal 解除所有 interruptible wait。
    // 2. group_exit status 已先发布，因此 sibling 在 trap return 不会把内部唤醒误报为 SIGKILL。
    for thread in threads.iter().filter(|thread| thread.tid() != current_tid) {
        thread
            .queue_signal(threads.iter(), 9, PendingSignal::kernel())
            .expect("kernel SIGKILL must be valid");
    }
    for thread in threads
        .into_iter()
        .filter(|thread| thread.tid() != current_tid)
    {
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
    let (
        removed,
        process_status,
        parent_waiter,
        init_waiter,
        parent_signal_pid,
        terminal_hangup_targets,
        orphaned_group_targets,
    ) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let exiting_pid = task.tgid();
        let process_will_exit = graph.nodes.get(&exiting_pid).is_some_and(
            |node| matches!(&node.state, ProcessState::Live(threads) if threads.len() == 1),
        );
        let orphaned_before = if process_will_exit {
            super::process_group::orphaned_stopped_groups(&graph)
        } else {
            BTreeMap::new()
        };
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
                assert!(node.waiter.is_none());
                node.state = ProcessState::Exited(status);
            }
            (removed, process_status, parent, session_leader)
        };
        assert!(Arc::ptr_eq(&removed, &task));

        match process_status {
            None => (removed, None, None, None, None, Vec::new(), Vec::new()),
            Some(status) => {
                // 1. orphan 只改写 graph 中的唯一 parent edge；不复制 child collection。
                if exiting_pid != INIT_PID {
                    for child in graph.nodes.values_mut() {
                        if child.parent == Some(exiting_pid) {
                            child.parent = Some(INIT_PID);
                        }
                    }
                }
                // 2. 取走 waiter owner 后释放 graph lock，再进入 scheduler seam，避免锁序反转。
                let parent_waiter = parent.and_then(|pid| {
                    graph
                        .nodes
                        .get_mut(&pid)
                        .and_then(|parent| parent.waiter.take())
                });
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
                let init_waiter = adopted_exited
                    .then(|| {
                        graph
                            .nodes
                            .get_mut(&INIT_PID)
                            .and_then(|init| init.waiter.take())
                    })
                    .flatten();
                // 1. process graph 是 SID/PGID membership owner，因此在同一 graph 临界区
                //    取走 Terminal foreground PGID 并冻结 live target 集合。
                // 2. 统一使用 graph -> Terminal 锁序；反向路径都会先释放 Terminal lock 再发 signal，
                //    否则 session exit 与 TIOCSPGRP 并发时可能形成锁环或错发给后加入成员。
                // 3. signal 在锁外发布，避免 generation 再次进入 process graph 造成自锁。
                let terminal_hangup_targets = if session_leader {
                    task.terminal()
                        .release_session(exiting_pid)
                        .map(|foreground| {
                            graph
                                .nodes
                                .iter()
                                .filter_map(|(&tgid, node)| {
                                    (node.session == exiting_pid
                                        && node.process_group == foreground
                                        && matches!(node.state, ProcessState::Live(_)))
                                    .then_some(tgid)
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let orphaned_after = super::process_group::orphaned_stopped_groups(&graph);
                let orphaned_group_targets = orphaned_after
                    .into_iter()
                    .filter_map(|(group, members)| {
                        (!orphaned_before.contains_key(&group)).then_some(members)
                    })
                    .collect();
                (
                    removed,
                    Some(status),
                    parent_waiter,
                    init_waiter,
                    parent_signal_pid,
                    terminal_hangup_targets,
                    orphaned_group_targets,
                )
            }
        }
    };

    // 退出导致的 terminal/orphan signal 必须先于 parent wake/SIGCHLD；否则 parent 可先
    // reap 并推进 shell 状态，使 POSIX exit consequences 的观察顺序依赖调度竞态。
    for tgid in terminal_hangup_targets {
        send_kernel_process_signal(tgid, 1, PendingSignal::kernel());
    }
    for members in orphaned_group_targets {
        for &tgid in &members {
            send_kernel_process_signal(tgid, 1, PendingSignal::kernel());
        }
        for tgid in members {
            send_kernel_process_signal(tgid, 18, PendingSignal::kernel());
        }
    }

    // 1. process graph 先注销 Thread owner，再发布 clear-child-tid completion。
    // 2. 若顺序相反，pthread_join 可在 graph 仍计数已退出 sibling 时返回，使紧随的
    //    single-thread-only fork/exec 错误观察到 EAGAIN。
    if let Some(address) = task.take_clear_child_tid()
        && task.copy_to_user(address, &0u32.to_ne_bytes()).is_ok()
    {
        futex_wake(task.tgid(), address, 1);
    }
    if process_status.is_some() {
        task.close_all_files();
    }
    drop(removed);
    if process_status.is_none() {
        task.remove_thread_trap_context();
    }
    for waiter in [parent_waiter, init_waiter].into_iter().flatten() {
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
