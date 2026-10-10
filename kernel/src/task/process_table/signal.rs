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

mod child_notification;
pub(super) use child_notification::{
    ChildNotificationScope, ParentSignalWake, PreparedChildNotifications, exit_notification_signal,
    lock_child_notifications, prepare_parent_signal_wake, stage_child_signal,
};

/// 冻结 caller Thread group 后原子修改 disposition 和所有同组 pending。
/// 查询无需 graph；replacement 不允许取得/保活其他 Process 的 pending owner。
pub(in crate::task) fn signal_action_for_group(
    task: &TaskControlBlock,
    signal: usize,
    replacement: Option<crate::task::SignalAction>,
) -> Result<crate::task::SignalAction, ()> {
    if replacement.is_none() {
        return task
            .signal_action_for_threads(core::iter::empty(), signal, None)
            .map(|(action, retired)| {
                drop(retired);
                action
            });
    }
    let graph = PROCESS_TABLE.graph.lock();
    let Some(ProcessState::Live(threads)) = graph.nodes.get(&task.tgid()).map(|node| &node.state)
    else {
        return Err(());
    };
    let mut result = task.signal_action_for_threads(threads.values(), signal, replacement);
    if let Ok((_, retired)) = &mut result {
        retired
            .recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
    }
    drop(graph);
    if result.is_ok() {
        resume_ignored_posix_timers(task.tgid(), signal);
    }
    result.map(|(action, retired)| {
        drop(retired);
        action
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalSendError {
    InvalidSignal,
    NotFound,
    Permission,
    Again,
}

/// native pidfd_send_signal 的 PID association scope；不重新按 visible number 选择目标。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PidSignalScope {
    Thread,
    ThreadGroup,
    ProcessGroup,
}

pub(crate) fn send_pid_signal(
    identity: &Arc<crate::namespace::PidIdentity>,
    scope: PidSignalScope,
    signal: usize,
    info: Option<PendingSignal>,
) -> Result<(), SignalSendError> {
    let sender = current_task().ok_or(SignalSendError::NotFound)?;
    if info.is_some_and(|info| info.restricted_user_source())
        && (scope == PidSignalScope::ProcessGroup || !Arc::ptr_eq(&sender.tid_identity(), identity))
    {
        return Err(SignalSendError::Permission);
    }
    {
        let graph = PROCESS_TABLE.graph.lock();
        let valid = match scope {
            PidSignalScope::Thread => {
                graph
                    .nodes
                    .get(&identity.key())
                    .is_some_and(|node| Arc::ptr_eq(node.pid.identity(), identity))
                    || graph
                        .threads
                        .get(&identity.key())
                        .is_some_and(|index| Arc::ptr_eq(index.pid.identity(), identity))
                    || graph
                        .kernel_threads
                        .get(&identity.key())
                        .is_some_and(|thread| Arc::ptr_eq(&thread.tid_identity(), identity))
            }
            PidSignalScope::ThreadGroup => {
                graph
                    .nodes
                    .get(&identity.key())
                    .is_some_and(|node| Arc::ptr_eq(node.pid.identity(), identity))
                    || graph
                        .threads
                        .get(&identity.key())
                        .is_some_and(|index| Arc::ptr_eq(index.pid.identity(), identity))
                    || graph
                        .kernel_threads
                        .get(&identity.key())
                        .is_some_and(|thread| Arc::ptr_eq(thread.tgid_identity(), identity))
            }
            PidSignalScope::ProcessGroup => graph
                .groups
                .get(&identity.key())
                .is_some_and(|group| Arc::ptr_eq(group.pgid.identity(), identity)),
        };
        if !valid {
            return Err(SignalSendError::NotFound);
        }
    }
    // stable graph key 永不复用；原 association 若在准备期间退休，既有 producer 返回 Gone。
    let origin = SignalOrigin::User { sender, info };
    match scope {
        PidSignalScope::Thread => send_selected_thread_signal(None, identity.key(), signal, origin),
        PidSignalScope::ThreadGroup => send_selected_processes(
            ProcessSelector::ThreadGroupOfPid(identity.key()),
            signal,
            origin,
        )
        .map(|_| ()),
        PidSignalScope::ProcessGroup => {
            send_selected_processes(ProcessSelector::Group(identity.key()), signal, origin)
                .map(|_| ())
        }
    }
}

#[derive(Clone, Copy)]
enum ProcessSelector {
    Process(usize),
    /// PIDFD 默认/覆盖 TGID scope 从被引用的 actual Thread 选择其 group；DAC 仍查询该 Thread。
    ThreadGroupOfPid(usize),
    Group(usize),
    AllExcept {
        caller: usize,
    },
}

/// 身份与来源的唯一 authority；显式 user siginfo 与 sender 一起固定，不可能走 kernel DAC 豁免。
enum SignalOrigin {
    User {
        sender: Arc<TaskControlBlock>,
        info: Option<PendingSignal>,
    },
    Kernel(PendingSignal),
}
impl SignalOrigin {
    fn caller(info: Option<PendingSignal>) -> Result<Self, SignalSendError> {
        Ok(Self::User {
            sender: current_task().ok_or(SignalSendError::NotFound)?,
            info,
        })
    }
    fn preparation_info(&self, default: PendingSignal) -> PendingSignal {
        match self {
            Self::Kernel(info)
            | Self::User {
                info: Some(info), ..
            } => *info,
            Self::User { info: None, .. } => default,
        }
    }
    fn sender(&self) -> Option<&Arc<TaskControlBlock>> {
        match self {
            Self::User { sender, .. } => Some(sender),
            Self::Kernel(_) => None,
        }
    }
}

fn project_user_queue(
    sender: &TaskControlBlock,
    target_pid_namespace: &crate::namespace::PidNamespace,
    target_user_namespace: &crate::namespace::UserNamespace,
    signal: usize,
    info: PendingSignal,
) -> PendingSignal {
    let uid = info.user_sender_uid(signal).map_or(0, |uid| {
        sender
            .user_namespace()
            .to_kernel(true, uid)
            .map_or(65534, |uid| target_user_namespace.display_id(true, uid))
    });
    info.project_user_identity(
        signal,
        uid,
        sender.tgid_identity().number_in(target_pid_namespace) == 0,
    )
}

struct GeneratedSignal {
    queued: bool,
    eligible: Option<Arc<TaskControlBlock>>,
    notification: Option<JobNotification>,
    retired: crate::task::model::RetiredSignals,
}

struct SelectedProcess {
    tgid: usize,
    result: SelectedProcessResult,
}

enum SelectedProcessResult {
    Denied,
    Probe,
    Generated(GeneratedSignal),
    Again,
    Gone,
}

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

/// 向指定 Thread 投递 kernel-generated signal，绕过 userspace credential 检查。
pub(crate) fn send_kernel_thread_signal(
    tgid: usize,
    tid: usize,
    signal: usize,
) -> Result<(), SignalSendError> {
    send_selected_thread_signal(
        Some(tgid),
        tid,
        signal,
        SignalOrigin::Kernel(PendingSignal::kernel()),
    )
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

fn send_selected_thread_signal(
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
        crate::task::model::PreparedSignalRecord::default,
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
            .queue_signal(threads.values(), signal, info, &mut prepared)
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
        crate::task::processor::request_task_reschedule(&target);
    }
    Ok(())
}

/// 按 Linux kill pid selector 向每个匹配 Process 发布一次 SI_USER signal。
///
/// # Parameters
///
/// - `pid`: `>0` 为 TGID，`0` 为 caller PGID，`-1` 为除 init/caller 外全部，`<-1` 为 PGID。
/// - `signal`: Linux signal number；零仅执行 existence/selection probe。
///
/// # Returns
///
/// 至少一个 live Process 匹配时成功。
///
/// # Errors
///
/// signal 非法或没有匹配 live Process。
pub(crate) fn send_process_signal(pid: i32, signal: usize) -> Result<(), SignalSendError> {
    let sender = current_task().ok_or(SignalSendError::NotFound)?;
    let caller = sender.tgid();
    let selector = match pid {
        value if value > 0 => ProcessSelector::Process(
            sender
                .resolve_pid(value as usize)
                .ok_or(SignalSendError::NotFound)?,
        ),
        0 => {
            let group = PROCESS_TABLE
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
            let number = value.checked_neg().ok_or(SignalSendError::NotFound)? as usize;
            ProcessSelector::Group(
                sender
                    .resolve_pid(number)
                    .ok_or(SignalSendError::NotFound)?,
            )
        }
    };
    send_selected_processes(selector, signal, SignalOrigin::User { sender, info: None }).map(|_| ())
}

/// 向一个 process group 的每个 live Process 投递一次 kernel-generated signal。
pub(super) fn send_process_group_signal(pgid: usize, signal: usize) -> usize {
    send_selected_processes(
        ProcessSelector::Group(pgid),
        signal,
        SignalOrigin::Kernel(PendingSignal::kernel()),
    )
    .unwrap_or(0)
}

/// 向一个指定 Process 发布 kernel-owned siginfo，例如 SIGCHLD。
pub(super) fn send_kernel_process_signal(tgid: usize, signal: usize, info: PendingSignal) -> bool {
    send_selected_processes(
        ProcessSelector::Process(tgid),
        signal,
        SignalOrigin::Kernel(info),
    )
    .is_ok()
}

fn send_selected_processes(
    selector: ProcessSelector,
    signal: usize,
    origin: SignalOrigin,
) -> Result<usize, SignalSendError> {
    if signal > 64 {
        return Err(SignalSendError::InvalidSignal);
    }
    let mut cursor = 0usize;
    let mut result = SelectionResult::new();
    let mut overflow = false;
    while let Some(selected) = select_and_generate_process_signal(selector, cursor, signal, &origin)
    {
        cursor = selected.tgid;
        let generated = match selected.result {
            SelectedProcessResult::Gone => continue,
            SelectedProcessResult::Again => {
                overflow = true;
                continue;
            }
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
        drop(generated.retired);
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
        SelectionOutcome::NotFound if overflow => Err(SignalSendError::Again),
        SelectionOutcome::NotFound => Err(SignalSendError::NotFound),
    }
}

fn select_and_generate_process_signal(
    selector: ProcessSelector,
    after: usize,
    signal: usize,
    origin: &SignalOrigin,
) -> Option<SelectedProcess> {
    let sender = origin.sender();
    let mut graph = PROCESS_TABLE.graph.lock();
    // 精确 PID 与 PGID 查询各自实际 AVL owner；只有 kill(-1) 扫描可见 Process。
    let tgid = match selector {
        ProcessSelector::ThreadGroupOfPid(pid) => {
            let tgid = graph
                .threads
                .get(&pid)
                .map(|index| index.tgid)
                .or_else(|| graph.nodes.contains_key(&pid).then_some(pid))
                .or_else(|| graph.kernel_threads.contains_key(&pid).then_some(pid));
            tgid.filter(|&tgid| tgid > after)
        }
        ProcessSelector::Process(pid) => (pid > after
            && (graph.nodes.contains_key(&pid) || graph.kernel_threads.contains_key(&pid)))
        .then_some(pid),
        ProcessSelector::Group(pgid) => graph.groups.get(&pgid).and_then(|group| {
            group.members.iter_after(&after).find_map(|(&key, ())| {
                let node = graph.nodes.get(&key).expect("actual group member missing");
                sender
                    .is_none_or(|sender| {
                        node.pid.identity().number_in(&sender.pid_namespace()) != 0
                    })
                    .then_some(key)
            })
        }),
        ProcessSelector::AllExcept { caller } => {
            let user = graph.nodes.iter_after(&after).find_map(|(&key, node)| {
                (key != caller
                    && sender.is_some_and(|sender| {
                        node.pid.identity().number_in(&sender.pid_namespace()) > 1
                    }))
                .then_some(key)
            });
            let kernel = graph
                .kernel_threads
                .iter_after(&after)
                .find_map(|(&key, task)| {
                    (key != caller
                        && sender.is_some_and(|sender| {
                            task.tgid_identity().number_in(&sender.pid_namespace()) > 1
                        }))
                    .then_some(key)
                });
            match (user, kernel) {
                (Some(user), Some(kernel)) => Some(user.min(kernel)),
                (user, kernel) => user.or(kernel),
            }
        }
    }?;
    if let Some(kernel) = graph.kernel_threads.get(&tgid) {
        let result = if sender.is_some_and(|sender| !sender.may_signal_thread(kernel)) {
            SelectedProcessResult::Denied
        } else if signal == 0 {
            SelectedProcessResult::Probe
        } else {
            // actual kernel disposition policy 为 SIG_IGN；generation 完成但无 pending/wake。
            SelectedProcessResult::Generated(GeneratedSignal {
                queued: false,
                eligible: None,
                notification: None,
                retired: crate::task::model::RetiredSignals::default(),
            })
        };
        return Some(SelectedProcess { tgid, result });
    }
    let initial_signal_thread = match selector {
        ProcessSelector::ThreadGroupOfPid(pid) => {
            super::thread_selector::execution_by_tid(&graph, pid)
        }
        _ => None,
    };
    // 1. 先验证 DAC，拒绝路径不预占 recipient 配额；SIGCONT 保留同 session 的原生例外。
    if signal != 0
        && sender.is_some_and(|sender| {
            let selected = graph.nodes.get(&tgid).expect("selected node");
            !initial_signal_thread.as_ref().map_or_else(
                || sender.may_signal_process(&selected.process),
                |thread| sender.may_signal_thread(thread),
            ) && !(signal == syscall_abi::signal::SIGCONT
                && graph
                    .nodes
                    .get(&sender.tgid())
                    .is_some_and(|node| node.session == selected.session))
        })
    {
        return Some(SelectedProcess {
            tgid,
            result: SelectedProcessResult::Denied,
        });
    }
    // 2. 锁外准备期间 representative 可能退出/exec。只在同一 actual Process 中重新选取，
    // 不把 still-live group 当成 Gone，也不允许新 PID lifetime 吸收旧 record。
    let actual_process = graph
        .nodes
        .get(&tgid)
        .expect("selected node")
        .process
        .clone();
    let (prepared_target, mut prepared, mut child_records, selected_thread) = loop {
        if let ProcessSelector::ThreadGroupOfPid(pid) = selector
            && !graph.threads.contains_key(&pid)
            && !graph.nodes.contains_key(&pid)
        {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Gone,
            });
        }

        // 每次 preparation 都从同一实际 PID association 重新选择；exec de_thread 可移交 PID，
        // 旧 TCB pin 只保证 storage lifetime，不能证明仍是这个 PID 的接收者。
        let selected_thread = match selector {
            ProcessSelector::ThreadGroupOfPid(pid) => {
                super::thread_selector::execution_by_tid(&graph, pid)
            }
            _ => None,
        };
        let target = if signal != 0 && signal != syscall_abi::signal::SIGKILL {
            graph.nodes.get(&tgid).and_then(|node| match &node.state {
                ProcessState::Live(threads) => selected_thread
                    .clone()
                    .or_else(|| threads.values().next().cloned()),
                _ => None,
            })
        } else {
            None
        };
        let Some(target) = target else {
            break (
                None,
                crate::task::model::PreparedSignalRecord::default(),
                None,
                selected_thread,
            );
        };
        drop(graph);
        let preparation_info = origin.preparation_info(PendingSignal::process_directed(0));
        let prepared = if selected_thread.is_some() {
            target.prepare_signal_record(signal, preparation_info)
        } else {
            target.prepare_leader_signal_record(signal, preparation_info)
        };
        let parent_preparation = if signal == syscall_abi::signal::SIGCONT {
            let Some((records, owner)) =
                lock_child_notifications(tgid, ChildNotificationScope::Parent)
            else {
                return Some(SelectedProcess {
                    tgid,
                    result: SelectedProcessResult::Gone,
                });
            };
            graph = owner;
            Some(records)
        } else {
            graph = PROCESS_TABLE.graph.lock();
            None
        };
        let Some(node) = graph
            .nodes
            .get(&tgid)
            .filter(|node| Arc::ptr_eq(&node.process, &actual_process))
        else {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Gone,
            });
        };
        if match selector {
            ProcessSelector::ThreadGroupOfPid(pid) => {
                super::thread_selector::execution_by_tid(&graph, pid)
                    .is_some_and(|actual| Arc::ptr_eq(&actual, &target))
                    || selected_thread.is_none()
                        && super::thread_selector::execution_by_tid(&graph, pid).is_none()
                        && graph.nodes.contains_key(&pid)
                        && matches!(&node.state, ProcessState::Live(threads) if threads.get(&target.tid()).is_some_and(|actual| Arc::ptr_eq(actual, &target)))
            }
            _ => {
                matches!(&node.state, ProcessState::Live(threads) if threads.get(&target.tid()).is_some_and(|actual| Arc::ptr_eq(actual, &target)))
            }
        } {
            break (Some(target), prepared, parent_preparation, selected_thread);
        }
        // 3. 旧 preparation 先在 graph 外退还；下一轮重新捕获唯一 actual representative。
        drop(graph);
        drop(prepared);
        drop(parent_preparation);
        drop(target);
        drop(selected_thread);
        graph = PROCESS_TABLE.graph.lock();
        if !graph
            .nodes
            .get(&tgid)
            .is_some_and(|node| Arc::ptr_eq(&node.process, &actual_process))
        {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Gone,
            });
        }
    };
    if let ProcessSelector::ThreadGroupOfPid(pid) = selector {
        let actual_tgid = graph
            .threads
            .get(&pid)
            .map(|index| index.tgid)
            .or_else(|| graph.nodes.contains_key(&pid).then_some(pid));
        if actual_tgid != Some(tgid) {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Gone,
            });
        }
    }
    let recipient_user_namespace = selected_thread.as_ref().map_or_else(
        || actual_process.leader_user_namespace(),
        |thread| thread.user_namespace(),
    );
    let recipient_pid_namespace = actual_process.tgid_identity().namespace();
    let (eligible, queued, mut retired) = {
        let node = graph
            .nodes
            .get(&tgid)
            .expect("selected process disappeared under graph lock");
        let permitted = sender.is_none_or(|sender| {
            selected_thread.as_ref().map_or_else(
                || sender.may_signal_process(&node.process),
                |thread| sender.may_signal_thread(thread),
            ) || signal == syscall_abi::signal::SIGCONT
                && graph
                    .nodes
                    .get(&sender.tgid())
                    .is_some_and(|sender| sender.session == node.session)
        });
        if !permitted {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Denied,
            });
        }
        if signal == 0 {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Probe,
            });
        }
        let ProcessState::Live(threads) = &node.state else {
            // Linux zombie 的 PID/凭据仍在图中；合法 signal 返回成功而不生成 pending。
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Probe,
            });
        };
        let Some(representative) = prepared_target.as_ref().or_else(|| threads.values().next())
        else {
            drop(graph);
            return Some(SelectedProcess {
                tgid,
                result: SelectedProcessResult::Probe,
            });
        };
        let eligible = threads
            .values()
            .find(|thread| thread.accepts_process_signal(signal))
            .cloned();
        let info = match origin {
            SignalOrigin::Kernel(info) => *info,
            SignalOrigin::User {
                sender,
                info: Some(info),
            } => project_user_queue(
                sender,
                &recipient_pid_namespace,
                &recipient_user_namespace,
                signal,
                *info,
            ),
            SignalOrigin::User { sender, info: None } => {
                let number = sender.tgid_identity().number_in(&recipient_pid_namespace);
                PendingSignal::process_directed(number as usize).with_identity(
                    number,
                    recipient_user_namespace.display_id(true, sender.credential_id(true, false)),
                    number == 0,
                )
            }
        };
        let generation = if selected_thread.is_some() {
            representative.queue_process_signal(threads.values(), signal, info, &mut prepared)
        } else {
            representative.queue_leader_process_signal(
                threads.values(),
                signal,
                info,
                &mut prepared,
            )
        };
        let (queued, retired) = match generation {
            Ok(queued) => queued,
            Err(()) => {
                // prepared storage 必须晚于 graph guard 析构；含原 recipient quota token。
                drop(graph);
                return Some(SelectedProcess {
                    tgid,
                    result: SelectedProcessResult::Again,
                });
            }
        };
        (eligible, queued, retired)
    };
    retired.recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
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
    drop(graph);
    Some(SelectedProcess {
        tgid,
        result: SelectedProcessResult::Generated(GeneratedSignal {
            queued,
            eligible,
            notification,
            retired,
        }),
    })
}

/// POSIX expiry 只移动创建时的实际预留 record；不走动态 RT preparation 或 kill fallback。
/// graph → timer 复查后释放 timer guard，才进入 disposition/pending owner。
pub(super) fn publish_posix_timer_signal(expired: super::timer_queue::ExpiredPosixTimer) {
    use super::timer_queue::PosixTimerNotification;
    let (signal, value, directed_tid) = match expired.notification {
        PosixTimerNotification::Process { signal, value } => (signal, value, None),
        PosixTimerNotification::Thread { tid, signal, value } => (signal, value, Some(tid)),
        PosixTimerNotification::Default | PosixTimerNotification::None => {
            unreachable!("normalized notification")
        }
    };
    let mut retired = crate::task::model::RetiredSignals::default();
    let publication = {
        let (mut child_records, mut graph) = if signal == syscall_abi::signal::SIGCONT {
            let Some((records, graph)) =
                lock_child_notifications(expired.tgid, ChildNotificationScope::Parent)
            else {
                return;
            };
            (Some(records), graph)
        } else {
            (None, PROCESS_TABLE.graph.lock())
        };
        let Some(ProcessState::Live(threads)) =
            graph.nodes.get(&expired.tgid).map(|node| &node.state)
        else {
            return;
        };
        let target = match directed_tid {
            Some(tid) => threads.get(&tid).filter(|target| {
                expired
                    .thread_target
                    .as_ref()
                    .is_some_and(|expected| Arc::ptr_eq(expected, &target.tid_identity()))
            }),
            None => threads.values().next(),
        }
        .cloned();
        let Some(target) = target else { return };
        let Some(mut prepared) = PROCESS_TABLE
            .timers
            .lock()
            .prepare_posix_notification(&expired)
        else {
            return;
        };
        // 同一 timer 的旧 record 尚在 pending：只更新其有效 sequence，不增加第二个事件。
        if !prepared.has_record() {
            return;
        }
        let info = PendingSignal::timer(expired.id, 0, value);
        let (queued, discarded) = match directed_tid {
            Some(_) => target.queue_signal(threads.values(), signal, info, &mut prepared),
            None => {
                target.queue_leader_process_signal(threads.values(), signal, info, &mut prepared)
            }
        }
        .expect("preallocated timer record is present");
        retired.append(discarded);
        retired
            .recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
        if !queued {
            PROCESS_TABLE.timers.lock().ignore_posix_expiry(&expired);
            // graph 保活实际 timer registry Arc；这里只归还 Box，不收费/析构最后 owner。
            prepared.recycle_timer();
        }
        let notification = if signal == syscall_abi::signal::SIGCONT {
            continue_process_locked(
                &mut graph,
                expired.tgid,
                child_records.as_mut().expect("SIGCONT child preparation"),
            )
        } else {
            if signal == syscall_abi::signal::SIGKILL {
                resume_for_fatal_signal_locked(&mut graph, expired.tgid);
            }
            None
        };
        // prepared 已全部转移或归还；其 Drop 不触碰 allocator/rlimit。
        (target, queued, notification)
    };
    drop(retired);
    let (target, queued, notification) = publication;
    publish_job_notification(notification);
    if !queued {
        return;
    }
    let woke = if directed_tid.is_some() {
        notify_signal_waiters(&target);
        wake_signal_waiter(&target)
    } else {
        wake_process_signal_waiter(expired.tgid)
    };
    if !woke && !interrupt_waiting_task(&target) {
        crate::task::processor::request_task_reschedule(&target);
    }
}

fn resume_ignored_posix_timers(tgid: usize, signal: usize) {
    let mut cursor = -1;
    loop {
        let expired = {
            let graph = PROCESS_TABLE.graph.lock();
            if !matches!(
                graph.nodes.get(&tgid).map(|node| &node.state),
                Some(ProcessState::Live(_))
            ) {
                return;
            }
            PROCESS_TABLE
                .timers
                .lock()
                .resume_ignored_posix(tgid, signal, cursor)
        };
        let Some(expired) = expired else { return };
        cursor = expired.id;
        publish_posix_timer_signal(expired);
    }
}

fn wake_process_signal_waiter(tgid: usize) -> bool {
    let mut cursor = 0usize;
    loop {
        let next = {
            let graph = PROCESS_TABLE.graph.lock();
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
        if cursor == 0 {
            notify_signal_waiters(&task);
        }
        cursor = tid;
        if wake_signal_waiter(&task) {
            return true;
        }
    }
}

/// signal 发布后从统一 registry 消费匹配的 `rt_sigtimedwait` registration。
fn wake_signal_waiter(task: &Arc<TaskControlBlock>) -> bool {
    let Some(wake) = WAIT_REGISTRY.wake_signal_registration(task) else {
        return false;
    };
    wake.claimed.is_none_or(|claimed| {
        assert!(Arc::ptr_eq(&claimed.task, task));
        crate::task::processor::wake_signal_task(claimed.task, claimed.id, WaitResult::Woken)
    })
}

/// signal publication 或 signalfd mask change 后，在 Graph/pending guard 外唤醒真实 sighand 源。
pub(crate) fn notify_signal_waiters(task: &TaskControlBlock) {
    use crate::file::FileReadinessContext;
    let Some(source) = task.signal_source() else {
        return;
    };
    source.changed();
    crate::file::Epoll::notify_signal_source(&source);
    while let Some(wake) = WAIT_REGISTRY.wake_signal_source_one(source.identity()) {
        if let Some(claimed) = wake.claimed {
            assert!(matches!(claimed.kind, IndexedWaitKind::Poll));
            let _ =
                crate::task::processor::wake_poll_task(claimed.task, claimed.id, WaitResult::Woken);
        }
    }
}

/// 从当前唯一 wait owner 取消目标 task 的 interruptible wait。
pub(super) fn interrupt_waiting_task(task: &Arc<TaskControlBlock>) -> bool {
    let key = match task.scheduling.state.lock().wait {
        Some(WaitMembership::TaskWait(key)) if key.interruptible() => Some(key),
        _ => None,
    };
    let event_wait = key.and_then(|key| {
        task.with_deliverable_signal_policy(key.killable(), || {
            (task.scheduling.state.lock().wait == Some(WaitMembership::TaskWait(key)))
                .then_some(key)
        })
        .flatten()
    });
    if let Some(key) = event_wait {
        if key.deadline().is_some() {
            return super::task_wait::wake_timed_task_event(
                task,
                key,
                WaitResult::Interrupted,
                false,
            );
        }
        return crate::task::processor::wake_waiting_task(
            task.clone(),
            WaitMembership::TaskWait(key),
            Some(WaitResult::Interrupted),
        );
    }
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
            IndexedWaitKind::TaskEvent(key) => crate::task::processor::wake_waiting_task(
                entry.task,
                WaitMembership::TaskWait(key),
                Some(interrupted),
            ),
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
            IndexedWaitKind::Signal { .. } => crate::task::processor::wake_signal_task(
                entry.task,
                wait_id,
                WaitResult::Interrupted,
            ),
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
        let mut graph = PROCESS_TABLE.graph.lock();
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

/// 标准 rt_sigqueueinfo 的精确 Process selector；user 提供的载荷保留独立 RT record。
pub(crate) fn send_queued_process_signal(
    pid: i32,
    signal: usize,
    info: PendingSignal,
) -> Result<(), SignalSendError> {
    let sender = current_task().ok_or(SignalSendError::NotFound)?;
    if info.restricted_user_source() && i64::from(pid) != i64::from(sender.visible_tid()) {
        return Err(SignalSendError::Permission);
    }
    let pid = u32::try_from(pid)
        .ok()
        .filter(|pid| *pid != 0)
        .and_then(|pid| sender.resolve_pid(pid as usize))
        .ok_or(SignalSendError::NotFound)?;
    send_selected_processes(
        ProcessSelector::Process(pid),
        signal,
        SignalOrigin::User {
            sender,
            info: Some(info),
        },
    )
    .map(|_| ())
}
/// 标准 rt_tgsigqueueinfo 的精确 TGID/TID selector，继续使用同一 thread generation/DAC。
pub(crate) fn send_queued_thread_signal(
    tgid: i32,
    tid: i32,
    signal: usize,
    info: PendingSignal,
) -> Result<(), SignalSendError> {
    if tgid <= 0 || tid <= 0 {
        return Err(SignalSendError::InvalidSignal);
    }
    let sender = current_task().ok_or(SignalSendError::NotFound)?;
    if info.restricted_user_source() && tid as u32 != sender.visible_tid() {
        return Err(SignalSendError::Permission);
    }
    let tgid = sender
        .resolve_pid(tgid as usize)
        .ok_or(SignalSendError::NotFound)?;
    let tid = sender
        .resolve_pid(tid as usize)
        .ok_or(SignalSendError::NotFound)?;
    send_selected_thread_signal(
        Some(tgid),
        tid,
        signal,
        SignalOrigin::User {
            sender,
            info: Some(info),
        },
    )
}
