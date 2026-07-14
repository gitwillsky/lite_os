use alloc::vec::Vec;
use core::ops::Bound::{Excluded, Unbounded};

use super::thread_selector::thread_by_tid;
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobControlState {
    Running,
    Stopping(usize),
    Stopped,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ChildEvents {
    pub(super) stopped: Option<usize>,
    pub(super) continued: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalSendError {
    InvalidSignal,
    NotFound,
    Permission,
}

#[derive(Clone, Copy)]
enum ProcessSelector {
    Process(usize),
    Group(usize),
    AllExcept { caller: usize },
}

struct JobNotification {
    parent: usize,
    waiters: Vec<Arc<TaskControlBlock>>,
    info: PendingSignal,
}

struct GeneratedSignal {
    queued: bool,
    eligible: Option<Arc<TaskControlBlock>>,
    notification: Option<JobNotification>,
}

#[derive(Clone, Copy)]
enum JobEvent {
    Stopped(usize),
    Continued,
}

/// @description 通过唯一 process graph 定位 Thread 并合并一个 thread-directed signal。
///
/// @param tgid 目标 Thread 所属 Process ID。
/// @param tid 目标 Thread ID。
/// @param signal Linux signal number；零仅执行存在性检查。
/// @return 目标存在且 signal 合法时返回 `Ok(())`。
/// @errors Process/Thread 不存在或 signal 非法时返回 `Err(())`。
pub(crate) fn send_thread_signal(
    tgid: usize,
    tid: usize,
    signal: usize,
) -> Result<(), SignalSendError> {
    send_selected_thread_signal(Some(tgid), tid, signal, None)
}

/// @description 向指定 Thread 投递 kernel-generated signal，绕过 userspace credential 检查。
pub(crate) fn send_kernel_thread_signal(
    tgid: usize,
    tid: usize,
    signal: usize,
) -> Result<(), SignalSendError> {
    send_selected_thread_signal(Some(tgid), tid, signal, Some(PendingSignal::kernel()))
}

/// @description 按全局 TID 定位 Thread，并复用唯一 thread-signal generation seam。
///
/// @param tid 目标 Thread ID。
/// @param signal Linux signal number；零只做 existence probe。
/// @return 目标存在且 signal 合法时返回 `Ok(())`。
/// @errors TID 不存在或 signal 非法时返回 `Err(())`。
pub(crate) fn send_tid_signal(tid: usize, signal: usize) -> Result<(), SignalSendError> {
    send_selected_thread_signal(None, tid, signal, None)
}

fn send_selected_thread_signal(
    expected_tgid: Option<usize>,
    tid: usize,
    signal: usize,
    kernel_info: Option<PendingSignal>,
) -> Result<(), SignalSendError> {
    let (target, queued, notification) = {
        let mut graph = TASK_MANAGER.graph.lock();
        let tgid = match expected_tgid {
            Some(tgid) => tgid,
            None => thread_by_tid(&graph, tid)
                .map(|(tgid, _)| tgid)
                .ok_or(SignalSendError::NotFound)?,
        };
        let Some(ProcessState::Live(threads)) = graph.nodes.get(&tgid).map(|node| &node.state)
        else {
            return Err(SignalSendError::NotFound);
        };
        let target = threads
            .get(&tid)
            .cloned()
            .ok_or(SignalSendError::NotFound)?;
        if kernel_info.is_none() {
            let sender_task = current_task().ok_or(SignalSendError::NotFound)?;
            let same_session = signal == 18
                && graph.nodes.get(&sender_task.tgid()).is_some_and(|sender| {
                    sender.session == graph.nodes.get(&tgid).unwrap().session
                });
            if !sender_task.may_signal(&target) && !same_session {
                return Err(SignalSendError::Permission);
            }
        }
        if signal == 0 {
            return Ok(());
        }
        let all_threads = threads.values().cloned().collect::<Vec<_>>();
        let sender = current_task().map_or(0, |task| task.tgid());
        let info = kernel_info.unwrap_or_else(|| PendingSignal::thread_directed(sender));
        let queued = if target.ignores_generated_signal_as_init(signal) {
            false
        } else {
            target
                .queue_signal(all_threads.iter(), signal, info)
                .map_err(|()| SignalSendError::InvalidSignal)?;
            true
        };
        let notification = if signal == 18 {
            continue_process_locked(&mut graph, tgid)
        } else {
            if signal == 9 {
                resume_for_fatal_signal_locked(&mut graph, tgid);
            }
            None
        };
        (target, queued, notification)
    };
    publish_job_notification(notification);
    // 1. 未命中 wait membership 的 Running target 必须显式进入调度点；否则移除周期性
    // tick yield 后，纯用户态远端线程可能无限期不观察 pending signal。
    if queued && !wake_signal_waiter(&target) && !interrupt_waiting_task(&target) {
        crate::task::processor::request_task_reschedule(&target);
    }
    Ok(())
}

/// @description 按 Linux kill pid selector 向每个匹配 Process 发布一次 SI_USER signal。
///
/// @param pid `>0` 为 TGID，`0` 为 caller PGID，`-1` 为除 init/caller 外全部，`<-1` 为 PGID。
/// @param signal Linux signal number；零仅执行 existence/selection probe。
/// @return 至少一个 live Process 匹配时成功。
/// @errors signal 非法或没有匹配 live Process。
pub(crate) fn send_process_signal(pid: i32, signal: usize) -> Result<(), SignalSendError> {
    let caller = current_task().ok_or(SignalSendError::NotFound)?.tgid();
    let selector = match pid {
        value if value > 0 => ProcessSelector::Process(value as usize),
        0 => {
            let group = TASK_MANAGER
                .graph
                .lock()
                .nodes
                .get(&caller)
                .map(|node| node.process_group)
                .ok_or(SignalSendError::NotFound)?;
            ProcessSelector::Group(group)
        }
        -1 => ProcessSelector::AllExcept { caller },
        value => {
            ProcessSelector::Group(value.checked_neg().ok_or(SignalSendError::NotFound)? as usize)
        }
    };
    let info = PendingSignal::process_directed(caller);
    send_selected_processes(selector, signal, info, current_task()).map(|_| ())
}

/// @description 向一个 process group 的每个 live Process 投递一次 kernel-generated signal。
pub(super) fn send_process_group_signal(pgid: usize, signal: usize) -> usize {
    send_selected_processes(
        ProcessSelector::Group(pgid),
        signal,
        PendingSignal::kernel(),
        None,
    )
    .unwrap_or(0)
}

/// @description 向一个指定 Process 发布 kernel-owned siginfo，例如 SIGCHLD。
pub(super) fn send_kernel_process_signal(tgid: usize, signal: usize, info: PendingSignal) -> bool {
    send_selected_processes(ProcessSelector::Process(tgid), signal, info, None).is_ok()
}

fn send_selected_processes(
    selector: ProcessSelector,
    signal: usize,
    info: PendingSignal,
    sender: Option<Arc<TaskControlBlock>>,
) -> Result<usize, SignalSendError> {
    if signal > 64 {
        return Err(SignalSendError::InvalidSignal);
    }
    let mut cursor = 0usize;
    let mut delivered = 0usize;
    let mut denied = false;
    while let Some(tgid) = next_process(selector, cursor) {
        cursor = tgid;
        match process_signal_permitted(sender.as_ref(), tgid, signal) {
            Some(true) => {}
            Some(false) => {
                denied = true;
                continue;
            }
            None => continue,
        }
        delivered += 1;
        if signal == 0 {
            continue;
        }
        let generated = generate_process_signal(tgid, signal, info)?;
        publish_job_notification(generated.notification);
        // 2. process-directed signal 选择的 Running Thread 遵循同一显式抢占协议。
        if generated.queued
            && !wake_process_signal_waiter(tgid)
            && let Some(target) = generated.eligible
            && !interrupt_waiting_task(&target)
        {
            crate::task::processor::request_task_reschedule(&target);
        }
    }
    if delivered != 0 {
        Ok(delivered)
    } else if denied {
        Err(SignalSendError::Permission)
    } else {
        Err(SignalSendError::NotFound)
    }
}

fn process_signal_permitted(
    sender: Option<&Arc<TaskControlBlock>>,
    tgid: usize,
    signal: usize,
) -> Option<bool> {
    let Some(sender) = sender else {
        return Some(true);
    };
    let graph = TASK_MANAGER.graph.lock();
    let target_node = graph.nodes.get(&tgid)?;
    let target = (match &target_node.state {
        ProcessState::Live(threads) => threads.values().next(),
        _ => None,
    })?;
    Some(
        sender.may_signal(target)
            || signal == 18
                && graph
                    .nodes
                    .get(&sender.tgid())
                    .is_some_and(|node| node.session == target_node.session),
    )
}

fn next_process(selector: ProcessSelector, after: usize) -> Option<usize> {
    let graph = TASK_MANAGER.graph.lock();
    graph
        .nodes
        .range((Excluded(after), Unbounded))
        .find_map(|(&tgid, node)| {
            let matches = match selector {
                ProcessSelector::Process(pid) => tgid == pid,
                ProcessSelector::Group(pgid) => node.process_group == pgid,
                ProcessSelector::AllExcept { caller } => tgid > INIT_PID && tgid != caller,
            };
            if !matches {
                return None;
            }
            let ProcessState::Live(threads) = &node.state else {
                return None;
            };
            (!threads.is_empty()).then_some(tgid)
        })
}

fn generate_process_signal(
    tgid: usize,
    signal: usize,
    info: PendingSignal,
) -> Result<GeneratedSignal, SignalSendError> {
    let mut graph = TASK_MANAGER.graph.lock();
    let node = graph.nodes.get(&tgid).ok_or(SignalSendError::NotFound)?;
    let ProcessState::Live(threads) = &node.state else {
        return Err(SignalSendError::NotFound);
    };
    let all_threads = threads.values().cloned().collect::<Vec<_>>();
    let representative = all_threads
        .first()
        .cloned()
        .ok_or(SignalSendError::NotFound)?;
    let eligible = all_threads
        .iter()
        .find(|thread| thread.accepts_process_signal(signal))
        .cloned();
    let queued = if representative.ignores_generated_signal_as_init(signal) {
        false
    } else {
        representative
            .queue_process_signal(all_threads.iter(), signal, info)
            .map_err(|()| SignalSendError::InvalidSignal)?
    };
    let notification = if signal == 18 {
        continue_process_locked(&mut graph, tgid)
    } else {
        if signal == 9 {
            resume_for_fatal_signal_locked(&mut graph, tgid);
        }
        None
    };
    Ok(GeneratedSignal {
        queued,
        eligible,
        notification,
    })
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
pub(super) fn complete_process_stop(tgid: usize) {
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

fn continue_process_locked(graph: &mut ProcessGraph, tgid: usize) -> Option<JobNotification> {
    let (event, threads) = {
        let node = graph.nodes.get_mut(&tgid)?;
        let ProcessState::Live(live) = &node.state else {
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
        (event, live.values().cloned().collect::<Vec<_>>())
    };
    for thread in threads {
        crate::task::processor::continue_stopped_task(thread);
    }
    event.and_then(|event| {
        let info = match event {
            JobEvent::Stopped(signal) => PendingSignal::child_stopped(tgid, signal),
            JobEvent::Continued => PendingSignal::child_continued(tgid),
        };
        take_parent_notification(graph, tgid, info)
    })
}

fn resume_for_fatal_signal_locked(graph: &mut ProcessGraph, tgid: usize) {
    let threads = {
        let Some(node) = graph.nodes.get_mut(&tgid) else {
            return;
        };
        let ProcessState::Live(live) = &node.state else {
            return;
        };
        node.job_control = JobControlState::Running;
        live.values().cloned().collect::<Vec<_>>()
    };
    for thread in threads {
        crate::task::processor::continue_stopped_task(thread);
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

fn publish_job_notification(notification: Option<JobNotification>) {
    let Some(notification) = notification else {
        return;
    };
    for waiter in notification.waiters {
        crate::task::processor::wake_child_task(waiter, WaitResult::Woken);
    }
    send_kernel_process_signal(notification.parent, 17, notification.info);
}

fn wake_process_signal_waiter(tgid: usize) -> bool {
    let mut cursor = 0usize;
    loop {
        let next = {
            let graph = TASK_MANAGER.graph.lock();
            let Some(ProcessState::Live(threads)) = graph.nodes.get(&tgid).map(|node| &node.state)
            else {
                return false;
            };
            threads
                .range((Excluded(cursor), Unbounded))
                .next()
                .map(|(&tid, task)| (tid, task.clone()))
        };
        let Some((tid, task)) = next else {
            return false;
        };
        cursor = tid;
        if wake_signal_waiter(&task) {
            return true;
        }
    }
}

/// @description signal 发布后从统一 registry 消费匹配的 `rt_sigtimedwait` registration。
fn wake_signal_waiter(task: &Arc<TaskControlBlock>) -> bool {
    let waiter = {
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        let Some(WaitMembership::Signal(id)) = task.scheduling.state.lock().wait else {
            return false;
        };
        let Some(mask) = queue.signal_mask(id) else {
            return false;
        };
        task.with_pending_signal(mask, || queue.remove(id))
            .flatten()
    };
    waiter.is_some_and(|entry| {
        assert!(Arc::ptr_eq(&entry.task, task));
        crate::task::processor::wake_signal_task(entry.task, WaitResult::Woken)
    })
}

/// @description 从当前唯一 wait owner 取消目标 task 的 interruptible wait。
pub(super) fn interrupt_waiting_task(task: &Arc<TaskControlBlock>) -> bool {
    let indexed = {
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        task.with_deliverable_signal(|| {
            let membership = task.scheduling.state.lock().wait;
            match membership {
                Some(
                    wait @ (WaitMembership::Deadline(id)
                    | WaitMembership::Futex(id)
                    | WaitMembership::Console(id)
                    | WaitMembership::Signal(id)
                    | WaitMembership::Pipe(id)
                    | WaitMembership::AdvisoryLock(id)
                    | WaitMembership::Poll(id)),
                ) => queue.remove(id).map(|entry| (id, wait, entry)),
                _ => None,
            }
        })
        .flatten()
    };
    if let Some((wait_id, membership, entry)) = indexed {
        assert!(Arc::ptr_eq(&entry.task, task));
        return match (membership, entry.kind) {
            (WaitMembership::Deadline(id), IndexedWaitKind::Deadline) => {
                assert_eq!(id, wait_id);
                crate::task::processor::wake_deadline_task(
                    entry.task,
                    wait_id,
                    WaitResult::Interrupted,
                )
            }
            (WaitMembership::Futex(id), IndexedWaitKind::Futex { .. }) => {
                assert_eq!(id, wait_id);
                crate::task::processor::wake_futex_task(
                    entry.task,
                    wait_id,
                    WaitResult::Interrupted,
                )
            }
            (WaitMembership::Console(id), IndexedWaitKind::Console) => {
                assert_eq!(id, wait_id);
                crate::task::processor::wake_console_task(entry.task, wait_id)
            }
            (WaitMembership::Signal(id), IndexedWaitKind::Signal { .. }) => {
                assert_eq!(id, wait_id);
                crate::task::processor::wake_signal_task(entry.task, WaitResult::Interrupted)
            }
            (WaitMembership::Pipe(id), IndexedWaitKind::Pipe { .. }) => {
                assert_eq!(id, wait_id);
                crate::task::processor::wake_pipe_task(entry.task, wait_id, WaitResult::Interrupted)
            }
            (WaitMembership::AdvisoryLock(id), IndexedWaitKind::AdvisoryLock { .. }) => {
                super::advisory_lock::interrupt_waiter(entry, wait_id, id)
            }
            (WaitMembership::Poll(id), IndexedWaitKind::Poll) => {
                assert_eq!(id, wait_id);
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Interrupted)
            }
            _ => panic!("indexed wait kind diverged from task membership"),
        };
    }

    let child = {
        let mut graph = TASK_MANAGER.graph.lock();
        task.with_deliverable_signal(|| {
            let scheduling = task.scheduling.state.lock();
            if scheduling.wait != Some(WaitMembership::Child) {
                None
            } else {
                let waiter = graph
                    .nodes
                    .get_mut(&task.tgid())
                    .expect("waiting process disappeared from graph")
                    .child_waiters
                    .remove(&task.tid());
                if let Some(waiter) = &waiter {
                    assert!(Arc::ptr_eq(waiter, task));
                }
                waiter
            }
        })
        .flatten()
    };
    child.is_some_and(|waiter| {
        crate::task::processor::wake_child_task(waiter, WaitResult::Interrupted)
    })
}
