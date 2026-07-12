use alloc::{sync::Arc, vec::Vec};

use super::*;

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
        release_terminal_session,
    ) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let exiting_pid = task.tgid();
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
            None => (removed, None, None, None, None, false),
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
                (
                    removed,
                    Some(status),
                    parent_waiter,
                    init_waiter,
                    parent_signal_pid,
                    session_leader,
                )
            }
        }
    };

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
    if release_terminal_session {
        task.terminal().release_session(task.tgid());
    }

    let idle_task_cx_ptr = with_current_processor(Processor::idle_context_ptr);
    let task_cx_ptr = {
        let mut task_cx = task.task_context().lock();
        &mut *task_cx as *mut TaskContext
    };

    crate::task::processor::defer_task_reap(task);
    // SAFETY: deferred owner keeps the exiting task stack/context alive through the switch;
    // idle context is hart-local and remains valid for the kernel lifetime.
    unsafe { crate::task::__switch(task_cx_ptr, idle_task_cx_ptr) };
    panic!("exited task context resumed")
}
