//! Thread-directed signal 的选择、授权、generation 与唤醒事务。
use super::*;

/// 通过唯一 process graph 定位 Thread 并合并一个 thread-directed signal。
///
/// # Parameters
///
/// - `tgid`: 目标 Thread 所属 Process ID。
/// - `tid`: 目标 Thread ID。
/// - `signal`: Linux signal number；零仅执行存在性检查。
///
/// # Returns
///
/// 目标存在且 signal 合法时返回 `Ok(())`。
///
/// # Errors
///
/// Process/Thread 不存在或 signal 非法时返回 `Err(())`。
pub(crate) fn send_thread_signal(
    tgid: usize,
    tid: usize,
    signal: usize,
) -> Result<(), SignalSendError> {
    send_selected_thread_signal(Some(tgid), tid, signal, SignalOrigin::caller(None)?)
}

/// 投递内核已授权的 Thread 信号，复用真实 generation 与 pending owner。
///
/// # Parameters
/// - `tgid`: 目标 Process key。
/// - `tid`: 目标 Thread key。
/// - `signal`: 标准信号号，零仅检查存在性。
/// - `info`: 已构造/投影的来源信息。
///
/// # Returns
/// 成功返回 unit。
///
/// # Errors
/// 目标消失、信号非法或排队失败时返回 SignalSendError。
pub(crate) fn send_kernel_thread_signal(
    tgid: usize,
    tid: usize,
    signal: usize,
    info: PendingSignal,
) -> Result<(), SignalSendError> {
    send_selected_thread_signal(Some(tgid), tid, signal, SignalOrigin::Kernel(info))
}

/// 按全局 TID 定位 Thread，并复用唯一 thread-signal generation seam。
///
/// # Parameters
///
/// - `tid`: 目标 Thread ID。
/// - `signal`: Linux signal number；零只做 existence probe。
///
/// # Returns
///
/// 目标存在且 signal 合法时返回 `Ok(())`。
///
/// # Errors
///
/// TID 不存在或 signal 非法时返回 `Err(())`。
pub(crate) fn send_tid_signal(tid: usize, signal: usize) -> Result<(), SignalSendError> {
    send_selected_thread_signal(None, tid, signal, SignalOrigin::caller(None)?)
}

/// 在 graph 事务中选择目标并授权、生成和唤醒 Thread-directed 信号。
///
/// # Parameters
/// - `expected_tgid`: tgkill 的组约束；None 为仅按 TID 选择。
/// - `tid`: 目标内部稳定 key。
/// - `signal`: 标准信号号，零仅探测。
/// - `origin`: caller 凭据及 payload，或内核已授权的 payload。
///
/// # Returns
/// 存在且合法时返回 unit，包括目标 policy 忽略信号的情况。
///
/// # Errors
/// 目标消失、组不匹配、凭据拒绝或排队失败时返回对应 SignalSendError。
pub(super) fn send_selected_thread_signal(
    expected_tgid: Option<usize>,
    tid: usize,
    signal: usize,
    origin: SignalOrigin,
) -> Result<(), SignalSendError> {
    if signal > 64 {
        return Err(SignalSendError::InvalidSignal);
    }
    let prepared_target = if signal != 0 && signal != syscall_abi::signal::SIGKILL {
        let graph = PROCESS_TABLE.graph.lock();
        match thread_by_tid(&graph, tid) {
            Some((tgid, target)) => {
                if expected_tgid.is_some_and(|expected| expected != tgid) {
                    return Err(SignalSendError::NotFound);
                }
                if origin.sender().is_some_and(|sender| {
                    !sender.may_signal_thread(&target)
                        && !(signal == syscall_abi::signal::SIGCONT
                            && graph.nodes.get(&sender.tgid()).is_some_and(|node| {
                                node.session
                                    == graph.nodes.get(&tgid).expect("target process").session
                            }))
                }) {
                    return Err(SignalSendError::Permission);
                }
                Some(target)
            }
            None => None,
        }
    } else {
        None
    };
    let mut prepared = prepared_target.as_ref().map_or_else(
        crate::execution::task::model::PreparedSignalRecord::default,
        |target| {
            target.prepare_signal_record(
                signal,
                origin.preparation_info(PendingSignal::thread_directed(0)),
            )
        },
    );
    let (target, queued, notification, retired) = {
        let (mut child_records, mut graph) = if let Some(target) = prepared_target
            .as_ref()
            .filter(|_| signal == syscall_abi::signal::SIGCONT)
        {
            let (records, graph) =
                lock_child_notifications(target.tgid(), ChildNotificationScope::Parent)
                    .ok_or(SignalSendError::NotFound)?;
            (Some(records), graph)
        } else {
            (None, PROCESS_TABLE.graph.lock())
        };
        if let Some(kernel) = graph.kernel_threads.get(&tid) {
            if expected_tgid.is_some_and(|tgid| tgid != tid) {
                return Err(SignalSendError::NotFound);
            }
            if origin
                .sender()
                .is_some_and(|sender| !sender.may_signal_thread(kernel))
            {
                return Err(SignalSendError::Permission);
            }
            // 标准 boot workers 继承 ignore_signals policy；存在/权限正确后忽略，不创建用户 pending。
            return Ok(());
        }
        let tgid = match expected_tgid {
            Some(tgid) => tgid,
            None => graph
                .threads
                .get(&tid)
                .map(|index| index.tgid)
                .or_else(|| graph.nodes.get(&tid).map(|_| tid))
                .ok_or(SignalSendError::NotFound)?,
        };
        if tgid == tid
            && let Some(node) = graph.nodes.get(&tgid)
            && !matches!(&node.state, ProcessState::Live(threads) if threads.contains_key(&tid))
        {
            // 原 leader zombie 仍有实际 PID association，合法 thread signal 不创建 pending。
            let same_session = signal == syscall_abi::signal::SIGCONT
                && origin.sender().is_some_and(|sender| {
                    graph
                        .nodes
                        .get(&sender.tgid())
                        .is_some_and(|sender| sender.session == node.session)
                });
            if origin
                .sender()
                .is_some_and(|sender| !sender.may_signal_process(&node.process))
                && !same_session
            {
                return Err(SignalSendError::Permission);
            }
            return Ok(());
        }
        if let Some(target) = graph
            .threads
            .get(&tid)
            .and_then(|index| index.retiring_task.as_ref())
        {
            let same_session = signal == syscall_abi::signal::SIGCONT
                && origin.sender().is_some_and(|sender| {
                    graph
                        .nodes
                        .get(&sender.tgid())
                        .zip(graph.nodes.get(&tgid))
                        .is_some_and(|(sender, target)| sender.session == target.session)
                });
            if origin
                .sender()
                .is_some_and(|sender| !sender.may_signal_thread(target))
                && !same_session
            {
                return Err(SignalSendError::Permission);
            }
            return Ok(());
        }
        let Some(ProcessState::Live(threads)) = graph.nodes.get(&tgid).map(|node| &node.state)
        else {
            return Err(SignalSendError::NotFound);
        };
        let target = threads
            .get(&tid)
            .cloned()
            .ok_or(SignalSendError::NotFound)?;
        if signal != 0
            && signal != syscall_abi::signal::SIGKILL
            && prepared_target
                .as_ref()
                .is_none_or(|original| !Arc::ptr_eq(original, &target))
        {
            return Err(SignalSendError::NotFound);
        }
        if let Some(sender_task) = origin.sender() {
            let same_session = signal == syscall_abi::signal::SIGCONT
                && graph.nodes.get(&sender_task.tgid()).is_some_and(|sender| {
                    sender.session == graph.nodes.get(&tgid).unwrap().session
                });
            if !sender_task.may_signal_thread(&target) && !same_session {
                return Err(SignalSendError::Permission);
            }
        }
        if signal == 0 {
            return Ok(());
        }
        let info = match &origin {
            SignalOrigin::Kernel(info) => *info,
            SignalOrigin::User {
                sender,
                info: Some(info),
            } => project_user_queue(
                sender,
                &target.pid_namespace(),
                &target.user_namespace(),
                signal,
                *info,
            ),
            SignalOrigin::User { sender, info: None } => {
                let number = sender.tgid_identity().number_in(&target.pid_namespace());
                PendingSignal::thread_directed(number as usize).with_identity(
                    number,
                    target
                        .user_namespace()
                        .display_id(true, sender.credential_id(true, false)),
                    number == 0,
                )
            }
        };
        let (queued, mut retired) = target
            .queue_signal(
                threads.values(),
                signal,
                super::ptrace::thread_traced(&graph, target.tid()),
                info,
                &mut prepared,
            )
            .map_err(|()| SignalSendError::Again)?;
        retired
            .recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
        let notification = if signal == syscall_abi::signal::SIGCONT {
            continue_process_locked(
                &mut graph,
                tgid,
                child_records.as_mut().expect("SIGCONT child preparation"),
            )
        } else {
            if signal == syscall_abi::signal::SIGKILL {
                resume_for_fatal_signal_locked(&mut graph, tgid);
            }
            None
        };
        (target, queued, notification, retired)
    };
    drop(retired);
    publish_job_notification(notification);
    // 1. 未命中 wait membership 的 Running target 必须显式进入调度点；否则移除周期性
    // tick yield 后，纯用户态远端线程可能无限期不观察 pending signal。
    if queued {
        notify_signal_waiters(&target);
    }
    if queued && !wake_signal_waiter(&target) && !interrupt_waiting_task(&target) {
        crate::execution::task::processor::request_task_reschedule(&target);
    }
    Ok(())
}
