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

mod thread;
use thread::send_selected_thread_signal;
pub(crate) use thread::{send_kernel_thread_signal, send_thread_signal, send_tid_signal};

mod wait;
pub(super) use wait::interrupt_waiting_task;
pub(crate) use wait::notify_signal_waiters;

mod timer;
pub(super) use timer::publish_posix_timer_signal;
use timer::resume_ignored_posix_timers;

mod disposition;
pub(in crate::task) use disposition::signal_action_for_group;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalSendError {
    InvalidSignal,
    NotFound,
    Permission,
    Again,
}

mod pidfd;
pub(crate) use pidfd::{PidSignalScope, send_pid_signal};

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
    group_exit_started: bool,
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
        if generated.group_exit_started {
            super::process_exit::wake_exiting_group(selected.tgid, None);
            continue;
        }
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
                group_exit_started: false,
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
            ) && !(signal == linux_abi::signal::SIGCONT
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
        let target = if signal != 0 && signal != linux_abi::signal::SIGKILL {
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
        let parent_preparation = if signal == linux_abi::signal::SIGCONT {
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
    let (eligible, queued, mut retired, default_fatal) = {
        let node = graph
            .nodes
            .get(&tgid)
            .expect("selected process disappeared under graph lock");
        let permitted = sender.is_none_or(|sender| {
            selected_thread.as_ref().map_or_else(
                || sender.may_signal_process(&node.process),
                |thread| sender.may_signal_thread(thread),
            ) || signal == linux_abi::signal::SIGCONT
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
            .find(|thread| {
                thread.accepts_process_signal(
                    signal,
                    super::ptrace::thread_traced(&graph, thread.tid()),
                )
            })
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
            representative.queue_process_signal(
                threads.values(),
                signal,
                super::ptrace::group_traced(&graph, tgid),
                info,
                &mut prepared,
            )
        } else {
            representative.queue_leader_process_signal(
                threads.values(),
                signal,
                super::ptrace::group_traced(&graph, tgid),
                info,
                &mut prepared,
            )
        };
        let (queued, retired, default_fatal) = match generation {
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
        (eligible, queued, retired, default_fatal)
    };
    let group_exit_started = queued
        && eligible.as_ref().is_some_and(|target| {
            complete_fatal_signal_locked(&mut graph, target, signal, default_fatal)
        });
    retired.recycle_timers(|event| PROCESS_TABLE.timers.lock().ignore_posix_notification(event));
    let notification = if signal == linux_abi::signal::SIGCONT {
        continue_process_locked(
            &mut graph,
            tgid,
            child_records.as_mut().expect("SIGCONT child preparation"),
        )
    } else {
        if signal == linux_abi::signal::SIGKILL {
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
            group_exit_started,
        }),
    })
}

/// 在同一 generation graph 事务中提交默认致命原因；锁外调用唯一 group wake 路径。
fn complete_fatal_signal_locked(
    graph: &mut ProcessGraph,
    target: &TaskControlBlock,
    signal: usize,
    default_fatal: bool,
) -> bool {
    default_fatal
        && super::process_exit::commit_group_exit(
            graph,
            target.tgid(),
            ProcessExitStatus::Signaled(signal as u8),
            None,
        )
        .is_some_and(|(_, initiated)| initiated)
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
