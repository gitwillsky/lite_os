use core::ops::Bound::{Excluded, Unbounded};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalSendError {
    InvalidSignal,
    NotFound,
}

#[derive(Clone, Copy)]
enum ProcessSelector {
    Process(usize),
    Group(usize),
    AllExcept { caller: usize },
}

/// @description 通过唯一 process graph 定位 Thread 并合并一个 thread-directed signal。
///
/// @param tgid 目标 Thread 所属 Process ID。
/// @param tid 目标 Thread ID。
/// @param signal Linux signal number；零仅执行存在性检查。
/// @return 目标存在且 signal 合法时返回 `Ok(())`。
/// @errors Process/Thread 不存在或 signal 非法时返回 `Err(())`。
pub(crate) fn send_thread_signal(tgid: usize, tid: usize, signal: usize) -> Result<(), ()> {
    let target = {
        let graph = TASK_MANAGER.graph.lock();
        let Some(ProcessState::Live(threads)) = graph.nodes.get(&tgid).map(|node| &node.state)
        else {
            return Err(());
        };
        threads.get(&tid).cloned().ok_or(())?
    };
    if signal == 0 {
        return Ok(());
    }
    let sender = current_task().map_or(0, |task| task.tgid());
    target.queue_signal(signal, PendingSignal::thread_directed(sender))?;
    wake_signal_waiter(&target);
    interrupt_waiting_task(&target);
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
    send_selected_processes(selector, signal, info).map(|_| ())
}

/// @description 向一个 process group 的每个 live Process 投递一次 kernel-generated signal。
pub(super) fn send_process_group_signal(pgid: usize, signal: usize) -> usize {
    send_selected_processes(
        ProcessSelector::Group(pgid),
        signal,
        PendingSignal::kernel(),
    )
    .unwrap_or(0)
}

/// @description 向一个指定 Process 发布 kernel-owned siginfo，例如 SIGCHLD。
pub(super) fn send_kernel_process_signal(tgid: usize, signal: usize, info: PendingSignal) -> bool {
    send_selected_processes(ProcessSelector::Process(tgid), signal, info).is_ok()
}

fn send_selected_processes(
    selector: ProcessSelector,
    signal: usize,
    info: PendingSignal,
) -> Result<usize, SignalSendError> {
    if signal > 64 {
        return Err(SignalSendError::InvalidSignal);
    }
    let mut cursor = 0usize;
    let mut delivered = 0usize;
    while let Some((tgid, representative, eligible)) = next_process(selector, signal, cursor) {
        cursor = tgid;
        delivered += 1;
        if signal == 0 {
            continue;
        }
        let queued = representative
            .queue_process_signal(signal, info)
            .map_err(|()| SignalSendError::InvalidSignal)?;
        if queued
            && !wake_process_signal_waiter(tgid)
            && let Some(target) = eligible
        {
            interrupt_waiting_task(&target);
        }
    }
    (delivered != 0)
        .then_some(delivered)
        .ok_or(SignalSendError::NotFound)
}

fn next_process(
    selector: ProcessSelector,
    signal: usize,
    after: usize,
) -> Option<(usize, Arc<TaskControlBlock>, Option<Arc<TaskControlBlock>>)> {
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
            let representative = threads.values().next()?.clone();
            let eligible = (signal != 0)
                .then(|| {
                    threads
                        .values()
                        .find(|thread| thread.accepts_process_signal(signal))
                        .cloned()
                })
                .flatten();
            Some((tgid, representative, eligible))
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
        let Some(entry) = queue.entries.get(&id) else {
            return false;
        };
        let IndexedWaitKind::Signal { mask } = entry.kind else {
            panic!("signal wait membership has divergent registry kind");
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
fn interrupt_waiting_task(task: &Arc<TaskControlBlock>) -> bool {
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
                    .waiter
                    .take();
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
