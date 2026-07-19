use super::thread_selector::thread_by_tid;
use super::*;

mod job_control;
mod selection_result;
pub(crate) use job_control::stop_current_process;
pub(super) use job_control::{ChildEvents, JobControlState, complete_process_stop};
use job_control::{
    JobNotification, continue_process_locked, publish_job_notification,
    resume_for_fatal_signal_locked,
};
use selection_result::{SelectionAttempt, SelectionOutcome, SelectionResult};

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

struct GeneratedSignal {
    queued: bool,
    eligible: Option<Arc<TaskControlBlock>>,
    notification: Option<JobNotification>,
}

struct SelectedProcess {
    tgid: usize,
    result: SelectedProcessResult,
}

enum SelectedProcessResult {
    Denied,
    Probe,
    Generated(GeneratedSignal),
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

/// 向指定 Thread 投递带领域 siginfo 的 kernel-generated signal。
pub(crate) fn send_kernel_thread_signal_info(
    tgid: usize,
    tid: usize,
    signal: usize,
    info: PendingSignal,
) -> Result<(), SignalSendError> {
    send_selected_thread_signal(Some(tgid), tid, signal, Some(info))
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
        let sender = current_task().map_or(0, |task| task.tgid());
        let info = kernel_info.unwrap_or_else(|| PendingSignal::thread_directed(sender));
        let queued = if target.ignores_generated_signal_as_init(signal) {
            false
        } else {
            target
                .queue_signal(threads.values(), signal, info)
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
    let mut result = SelectionResult::new();
    while let Some(selected) =
        select_and_generate_process_signal(selector, cursor, signal, info, sender.as_ref())
    {
        cursor = selected.tgid;
        let generated = match selected.result {
            SelectedProcessResult::Denied => {
                result.record(SelectionAttempt::Denied);
                continue;
            }
            SelectedProcessResult::Probe => {
                result.record(SelectionAttempt::Probe);
                continue;
            }
            SelectedProcessResult::Generated(generated) => generated,
        };
        result.record(SelectionAttempt::Generated);
        publish_job_notification(generated.notification);
        // 2. process-directed signal 选择的 Running Thread 遵循同一显式抢占协议。
        if generated.queued
            && !wake_process_signal_waiter(selected.tgid)
            && let Some(target) = generated.eligible
            && !interrupt_waiting_task(&target)
        {
            crate::task::processor::request_task_reschedule(&target);
        }
    }
    match result.finish() {
        SelectionOutcome::Success(delivered) => Ok(delivered),
        SelectionOutcome::Permission => Err(SignalSendError::Permission),
        SelectionOutcome::NotFound => Err(SignalSendError::NotFound),
    }
}

fn select_and_generate_process_signal(
    selector: ProcessSelector,
    after: usize,
    signal: usize,
    info: PendingSignal,
    sender: Option<&Arc<TaskControlBlock>>,
) -> Option<SelectedProcess> {
    let mut graph = TASK_MANAGER.graph.lock();
    let tgid = graph.nodes.iter_after(&after).find_map(|(&tgid, node)| {
        let selected = match selector {
            ProcessSelector::Process(pid) => tgid == pid,
            ProcessSelector::Group(pgid) => node.process_group == pgid,
            ProcessSelector::AllExcept { caller } => tgid > INIT_PID && tgid != caller,
        };
        (selected && matches!(&node.state, ProcessState::Live(threads) if !threads.is_empty()))
            .then_some(tgid)
    })?;
    let (eligible, queued) = {
        let node = graph
            .nodes
            .get(&tgid)
            .expect("selected process disappeared under graph lock");
        let ProcessState::Live(threads) = &node.state else {
            panic!("selected process stopped being live under graph lock");
        };
        let representative = threads
            .values()
            .next()
            .expect("selected process lost its representative")
            .clone();
        let permitted = sender.is_none_or(|sender| {
            sender.may_signal(&representative)
                || signal == 18
                    && graph
                        .nodes
                        .get(&sender.tgid())
                        .is_some_and(|sender| sender.session == node.session)
        });
        if !permitted {
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Denied,
            });
        }
        if signal == 0 {
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Probe,
            });
        }
        let eligible = threads
            .values()
            .find(|thread| thread.accepts_process_signal(signal))
            .cloned();
        let queued = if representative.ignores_generated_signal_as_init(signal) {
            false
        } else {
            representative
                .queue_process_signal(threads.values(), signal, info)
                .expect("validated process signal became invalid")
        };
        (eligible, queued)
    };
    let notification = if signal == 18 {
        continue_process_locked(&mut graph, tgid)
    } else {
        if signal == 9 {
            resume_for_fatal_signal_locked(&mut graph, tgid);
        }
        None
    };
    Some(SelectedProcess {
        tgid,
        result: SelectedProcessResult::Generated(GeneratedSignal {
            queued,
            eligible,
            notification,
        }),
    })
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
                .successor(&cursor)
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
    let Some(wake) = WAIT_REGISTRY.wake_signal_registration(task) else {
        return false;
    };
    wake.claimed.is_none_or(|claimed| {
        assert!(Arc::ptr_eq(&claimed.task, task));
        crate::task::processor::wake_signal_task(claimed.task, WaitResult::Woken)
    })
}

/// @description 从当前唯一 wait owner 取消目标 task 的 interruptible wait。
pub(super) fn interrupt_waiting_task(task: &Arc<TaskControlBlock>) -> bool {
    if task.has_deliverable_signal()
        && let Some(wake) = WAIT_REGISTRY.interrupt_task(task)
    {
        let Some(entry) = wake.claimed else {
            return true;
        };
        let wait_id = entry.id;
        let interrupted = WaitResult::Interrupted;
        assert!(Arc::ptr_eq(&entry.task, task));
        return match entry.kind {
            IndexedWaitKind::Deadline => crate::task::processor::wake_deadline_task(
                entry.task,
                wait_id,
                WaitResult::Interrupted,
            ),
            IndexedWaitKind::Futex { .. } => crate::task::processor::wake_futex_task(
                entry.task,
                wait_id,
                WaitResult::Interrupted,
            ),
            IndexedWaitKind::Console => {
                crate::task::processor::wake_console_task(entry.task, wait_id, interrupted)
            }
            IndexedWaitKind::Signal { .. } => {
                crate::task::processor::wake_signal_task(entry.task, WaitResult::Interrupted)
            }
            IndexedWaitKind::Pipe { .. } => {
                crate::task::processor::wake_pipe_task(entry.task, wait_id, WaitResult::Interrupted)
            }
            IndexedWaitKind::AdvisoryLock => {
                crate::task::processor::wake_flock_task(entry.task, wait_id, interrupted)
            }
            IndexedWaitKind::Poll => {
                crate::task::processor::wake_poll_task(entry.task, wait_id, WaitResult::Interrupted)
            }
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
