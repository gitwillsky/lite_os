use alloc::sync::Arc;

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::task::task_manager) enum JobControlState {
    Running,
    Stopping(usize),
    Stopped,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::task::task_manager) struct ChildEvents {
    pub(in crate::task::task_manager) stopped: Option<usize>,
    pub(in crate::task::task_manager) continued: bool,
}

pub(super) struct JobNotification {
    parent: usize,
    waiters: FallibleMap<usize, Arc<TaskControlBlock>>,
    info: PendingSignal,
}

#[derive(Clone, Copy)]
enum JobEvent {
    Stopped(usize),
    Continued,
}

/// @description 对默认 stop action 发起 Process group-stop，并阻塞当前 Thread 直到 SIGCONT。
///
/// @param signal 已从 pending queue 消费的 job-control stop signal。
/// @return SIGCONT 恢复当前 Thread 后返回；停止期间不占用 CPU。
pub(crate) fn stop_current_process(signal: usize) {
    let task = current_task().expect("group stop requires current task");
    let tgid = task.tgid();
    {
        let mut graph = TASK_MANAGER.graph.lock();
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("stopping process disappeared from graph");
        let ProcessState::Live(threads) = &node.state else {
            panic!("exited process attempted group stop");
        };
        node.job_control = JobControlState::Stopping(signal);
        for thread in threads.values() {
            crate::task::processor::request_task_stop(thread);
        }
    }
    drop(task);
    crate::task::suspend_current_and_run_next();
}

/// @description 最后一个 StopPending Thread 切回 idle 后提交 parent-visible stopped event。
///
/// @param tgid 刚完成 scheduler stop transition 的 Process ID。
/// @return stop 尚未完成或已通知时不执行操作。
pub(in crate::task::task_manager) fn complete_process_stop(tgid: usize) {
    let notification = {
        let mut graph = TASK_MANAGER.graph.lock();
        let completed_signal = {
            let Some(node) = graph.nodes.get(&tgid) else {
                return;
            };
            let JobControlState::Stopping(signal) = node.job_control else {
                return;
            };
            let ProcessState::Live(threads) = &node.state else {
                return;
            };
            threads
                .values()
                .all(|thread| {
                    matches!(
                        thread.scheduling.state.lock().run_state,
                        RunState::Stopped { .. }
                    )
                })
                .then_some(signal)
        };
        let Some(signal) = completed_signal else {
            return;
        };
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("completed stop process disappeared");
        node.job_control = JobControlState::Stopped;
        node.child_events.stopped = Some(signal);
        take_parent_notification(&mut graph, tgid, PendingSignal::child_stopped(tgid, signal))
    };
    publish_job_notification(notification);
}

pub(super) fn continue_process_locked(
    graph: &mut ProcessGraph,
    tgid: usize,
) -> Option<JobNotification> {
    let event = {
        let node = graph.nodes.get_mut(&tgid)?;
        let ProcessState::Live(_) = &node.state else {
            return None;
        };
        let event = match node.job_control {
            JobControlState::Running => None,
            JobControlState::Stopping(signal) => Some(JobEvent::Stopped(signal)),
            JobControlState::Stopped => Some(JobEvent::Continued),
        };
        node.job_control = JobControlState::Running;
        if let Some(event) = event {
            match event {
                JobEvent::Stopped(signal) => node.child_events.stopped = Some(signal),
                JobEvent::Continued => node.child_events.continued = true,
            }
        }
        event
    };
    let node = graph
        .nodes
        .get(&tgid)
        .expect("continued process disappeared");
    let ProcessState::Live(threads) = &node.state else {
        panic!("continued process stopped being live");
    };
    for thread in threads.values() {
        crate::task::processor::continue_stopped_task(thread.clone());
    }
    event.and_then(|event| {
        let info = match event {
            JobEvent::Stopped(signal) => PendingSignal::child_stopped(tgid, signal),
            JobEvent::Continued => PendingSignal::child_continued(tgid),
        };
        take_parent_notification(graph, tgid, info)
    })
}

pub(super) fn resume_for_fatal_signal_locked(graph: &mut ProcessGraph, tgid: usize) {
    {
        let Some(node) = graph.nodes.get_mut(&tgid) else {
            return;
        };
        let ProcessState::Live(_) = &node.state else {
            return;
        };
        node.job_control = JobControlState::Running;
    }
    let node = graph
        .nodes
        .get(&tgid)
        .expect("fatal-resumed process disappeared");
    let ProcessState::Live(threads) = &node.state else {
        panic!("fatal-resumed process stopped being live");
    };
    for thread in threads.values() {
        crate::task::processor::continue_stopped_task(thread.clone());
    }
}

fn take_parent_notification(
    graph: &mut ProcessGraph,
    child: usize,
    info: PendingSignal,
) -> Option<JobNotification> {
    let parent = graph.nodes.get(&child)?.parent?;
    let node = graph.nodes.get_mut(&parent)?;
    if !matches!(node.state, ProcessState::Live(_)) {
        return None;
    }
    Some(JobNotification {
        parent,
        waiters: take_child_waiters(node),
        info,
    })
}

pub(super) fn publish_job_notification(notification: Option<JobNotification>) {
    let Some(notification) = notification else {
        return;
    };
    let mut waiters = notification.waiters;
    while let Some((&tid, _)) = waiters.first_key_value() {
        let waiter = waiters.remove(&tid).expect("staged child waiter");
        crate::task::processor::wake_child_task(waiter, WaitResult::Woken);
    }
    send_kernel_process_signal(notification.parent, 17, notification.info);
}
