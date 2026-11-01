//! Linux 逐线程跟踪关系；graph 是 relation、stop、resume 与 wait claim 的唯一 owner。
use super::thread_selector::thread_by_tid;
use super::*;
use syscall_abi::signal::SIGKILL;

/// ptrace 的领域失败；ABI adapter 唯一映射到 errno。
#[derive(Debug, Clone, Copy)]
pub(crate) enum PtraceError {
    NotFound,
    Permission,
    Interrupted,
    OutOfMemory,
    Invalid,
    Io,
}

/// 已解码请求；用户地址/iovec 编解码由 syscall adapter 负责。
pub(crate) enum PtraceRequest<'a> {
    ReadWord(usize),
    WriteWord(usize, usize),
    GetRegisters(usize, &'a mut [u8]),
    SetRegisters(usize, &'a [u8]),
    GetSignalInfo(&'a mut [u8; 128]),
    SetSignalInfo { signal: usize, info: PendingSignal },
    Resume { signal: usize, detach: bool },
    Kill,
}

pub(super) struct Tracee {
    tracer: usize,
    tracer_group: usize,
    pub(super) tgid: usize,
    pub(super) pid: Arc<PidRegistration>,
    uid: u32,
    phase: Phase,
    claim: Option<usize>,
}

/// wait 的 immutable event lifetime；不保活 TCB/MM，也不要求 relation 继续存在。
#[derive(Clone)]
pub(super) struct TraceClaim {
    pub(super) pid: Arc<PidRegistration>,
    uid: u32,
}
impl core::fmt::Debug for TraceClaim {
    fn fmt(&self, output: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        output
            .debug_struct("TraceClaim")
            .field("pid", self.pid.identity())
            .field("uid", &self.uid)
            .finish()
    }
}

enum Phase {
    Running,
    Stopping {
        signal: usize,
        info: PendingSignal,
    },
    Stopped {
        signal: usize,
        info_signal: usize,
        info: PendingSignal,
        reported: bool,
    },
    Exited(i32),
}

fn wake_tracer(graph: &mut ProcessGraph, group: usize) {
    if let Some(node) = graph.nodes.get_mut(&group) {
        let mut waiters = take_child_waiters(node);
        while let Some((&tid, _)) = waiters.first_key_value() {
            let task = waiters.remove(&tid).expect("indexed tracer waiter");
            crate::execution::task::processor::wake_child_task(task, WaitResult::Woken);
        }
    }
}

/// 建立 TRACEME 或 ATTACH 关系，ATTACH 经真实 SIGSTOP 走 signal-delivery stop。
///
/// # Parameters
/// - `pid`: calling PID view 的目标 TID；None 为 TRACEME。
///
/// # Returns
/// 成功返回零；发布前所有 fallible storage 与 credential guard 已就绪。
///
/// # Errors
/// PID 不存在、同组/重复附加、凭据拒绝、被信号中断或分配失败。
pub(crate) fn ptrace_attach(pid: Option<usize>) -> Result<usize, PtraceError> {
    let caller = current_task().expect("ptrace requires caller");
    let tid = match pid {
        Some(pid) => caller.resolve_pid(pid).ok_or(PtraceError::NotFound)?,
        None => caller.tid(),
    };
    let target = {
        let graph = PROCESS_TABLE.graph.lock();
        thread_by_tid(&graph, tid).ok_or(PtraceError::NotFound)?.1
    };
    let slot =
        FallibleMap::<usize, Tracee>::try_reserve_node().map_err(|_| PtraceError::OutOfMemory)?;
    let membership =
        FallibleMap::<usize, ()>::try_reserve_node().map_err(|_| PtraceError::OutOfMemory)?;
    let pid_slot = PidLinkSlot::try_new().map_err(|_| PtraceError::OutOfMemory)?;
    let _credentials = target
        .credential_guard()
        .lock_killable()
        .map_err(|error| match error {
            crate::execution::sync::TaskMutexLockError::OutOfMemory => PtraceError::OutOfMemory,
            crate::execution::sync::TaskMutexLockError::Interrupted => PtraceError::Interrupted,
        })?;
    // claim token 与 relation 共用同一真实 PID association，wait copyout 不借可被 detach 删除的节点。
    let registration = {
        let graph = PROCESS_TABLE.graph.lock();
        graph
            .threads
            .get(&tid)
            .ok_or(PtraceError::NotFound)?
            .pid
            .clone_in(pid_slot)
    };
    let registration = Arc::try_new(registration).map_err(|_| PtraceError::OutOfMemory)?;
    let mut graph = PROCESS_TABLE.graph.lock();
    if graph.tracees.contains_key(&tid) {
        return Err(PtraceError::Permission);
    }
    if graph
        .threads
        .get(&tid)
        .is_none_or(|index| index.exit_in_progress)
    {
        return Err(PtraceError::NotFound);
    }
    let tracer = if pid.is_none() {
        let parent = graph
            .nodes
            .get(&target.tgid())
            .and_then(|node| node.parent_thread)
            .ok_or(PtraceError::Permission)?;
        thread_by_tid(&graph, parent)
            .ok_or(PtraceError::Permission)?
            .1
    } else {
        caller.clone()
    };
    if tracer.tgid() == target.tgid()
        || !(if pid.is_none() {
            tracer.may_trace_me(&target)
        } else {
            tracer.may_inspect_thread_real(&target)
        })
    {
        return Err(PtraceError::Permission);
    }
    graph
        .nodes
        .get_mut(&tracer.tgid())
        .expect("tracer group is live")
        .ptracees
        .commit_vacant(membership.fill(tid, ()));
    graph.tracees.commit_vacant(slot.fill(
        tid,
        Tracee {
            tracer: tracer.tid(),
            tracer_group: tracer.tgid(),
            tgid: target.tgid(),
            pid: registration,
            uid: target.credential_id(true, false),
            phase: Phase::Running,
            claim: None,
        },
    ));
    if pid.is_some() {
        // 1. relation 先于 SIGSTOP 发表，避免目标在 stop 到达前按 job-control 处理它。
        // 2. 使用 pending owner 的不可丢失非 RT slot，不在 graph 临界区分配。
        target.queue_ptrace_signal(syscall_abi::signal::SIGSTOP);
        crate::execution::task::processor::continue_stopped_task(target.clone());
        drop(graph);
        super::signal::interrupt_waiting_task(&target);
        crate::execution::task::processor::request_task_reschedule(&target);
    }
    Ok(0)
}

/// 检查 graph 内的真实 relation；generation 调用者已持有 graph，不创建 shadow flag。
pub(super) fn thread_traced(graph: &ProcessGraph, tid: usize) -> bool {
    graph.tracees.contains_key(&tid)
}
pub(super) fn group_traced(graph: &ProcessGraph, tgid: usize) -> bool {
    if graph.tracees.is_empty() {
        return false;
    }
    graph
        .nodes
        .get(&tgid)
        .is_some_and(|node| match &node.state {
            ProcessState::Live(threads) => threads
                .iter()
                .any(|(&tid, _)| graph.tracees.contains_key(&tid)),
            _ => graph.tracees.contains_key(&tgid),
        })
}

/// 截获已选中的信号并停止本 Thread，等待 tracer 决定投递/替换/抑制。
///
/// # Parameters
/// - `task`: 当前 Thread。
/// - `signal`: 已消费的未屏蔽信号。
/// - `info`: 同一事件的 siginfo。
/// - `queue`: 原 pending owner，供 blocked injection 归还。
///
/// # Returns
/// 抑制返回 None，否则返回最终信号与载荷；SIGKILL 永不被 tracer 延迟。
pub(in crate::execution::task) fn intercept_signal(
    task: &TaskControlBlock,
    signal: usize,
    info: PendingSignal,
    queue: super::super::model::SignalQueue,
) -> Option<(usize, PendingSignal)> {
    if signal == SIGKILL {
        return Some((signal, info));
    }
    // 1. 在 graph 内发表 stopping，再由真实 context-switch handoff 交付 stopped。
    {
        let mut graph = PROCESS_TABLE.graph.lock();
        let Some(tracee) = graph.tracees.get_mut(&task.tid()) else {
            return Some((signal, info));
        };
        task.prepare_ptrace_restart();
        tracee.phase = Phase::Stopping { signal, info };
        let target = thread_by_tid(&graph, task.tid())
            .expect("current tracee is live")
            .1;
        crate::execution::task::processor::request_task_stop(&target);
    }
    // context 保存完成的 post-switch handoff 才发表 stopped，寄存器不能与 running CPU 竞争。
    crate::execution::task::suspend_current_and_run_next();
    // 2. 原 signal frame 消费 tracer 唯一移交的决定，fatal signal 仍由退出路径优先处理。
    let resumed = task.take_ptrace_resume();
    if task.has_fatal_signal() {
        return None;
    }
    let (signal, info) = resumed.unwrap_or((signal, info));
    if signal == 0 {
        return None;
    }
    // 3. tracer 可改成当前被屏蔽的信号；归还原 pending owner，不能直接调用 handler 绕过 mask。
    if task.signal_mask(0, None).expect("read signal mask") & (1u64 << (signal - 1)) != 0 {
        match queue {
            super::super::model::SignalQueue::Thread => {
                let _ =
                    super::signal::send_kernel_thread_signal(task.tgid(), task.tid(), signal, info);
            }
            super::super::model::SignalQueue::Process => {
                super::signal::send_kernel_process_signal(task.tgid(), signal, info);
            }
        }
        return None;
    }
    Some((signal, info))
}

pub(super) fn complete_stop(task: &TaskControlBlock) {
    let notification = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let Some(tracee) = graph.tracees.get_mut(&task.tid()) else {
            return;
        };
        let Phase::Stopping { signal, info } = tracee.phase else {
            return;
        };
        tracee.uid = task.credential_id(true, false);
        tracee.phase = Phase::Stopped {
            signal,
            info_signal: signal,
            info,
            reported: false,
        };
        let group = tracee.tracer_group;
        let tracer_tid = tracee.tracer;
        wake_tracer(&mut graph, group);
        thread_by_tid(&graph, tracer_tid).map(|(_, tracer)| (group, tracer, signal))
    };
    if let Some((group, tracer, signal)) = notification {
        let pid = task.tid_identity().number_in(&tracer.pid_namespace());
        let uid = tracer
            .user_namespace()
            .display_id(true, task.credential_id(true, false));
        let info = PendingSignal::ptrace_stopped(pid, uid, signal);
        super::signal::send_kernel_process_signal(group, syscall_abi::signal::SIGCHLD, info);
    }
}

/// proc producer 只捕获真实 tracer identity；格式化与 PID view 投影归 fs。
pub(super) fn tracer_identity(tid: usize) -> Option<Arc<crate::namespace::PidIdentity>> {
    let graph = PROCESS_TABLE.graph.lock();
    graph
        .tracees
        .get(&tid)
        .and_then(|tracee| graph.threads.get(&tracee.tracer))
        .map(|index| index.pid.identity().clone())
}

/// 一个 syscall 固定的 attachment；user-copy 期间 detach/re-attach 不得换成同编号的新目标。
pub(crate) struct PtraceTarget {
    pid: Arc<PidRegistration>,
}

/// 在 user-copy 前解析并检查目标，保留实际 PID association 和 attachment identity。
///
/// # Parameters
/// - `pid`: caller PID namespace 中的目标 TID。
/// - `require_stopped`: 普通请求要求 ptrace-stop；仅 PTRACE_KILL 可以操作运行目标。
///
/// # Returns
/// 仅持 PID registration 的不可变 token，不保活 TCB/MM/files。
///
/// # Errors
/// 目标不存在、非 calling tracer group、要求停止但仍运行或 event 正被 claim 时返回 NotFound。
pub(crate) fn ptrace_target(
    pid: usize,
    require_stopped: bool,
) -> Result<PtraceTarget, PtraceError> {
    let caller = current_task().expect("ptrace target requires caller");
    let tid = caller.resolve_pid(pid).ok_or(PtraceError::NotFound)?;
    let graph = PROCESS_TABLE.graph.lock();
    let tracee = graph.tracees.get(&tid).ok_or(PtraceError::NotFound)?;
    if tracee.tracer_group != caller.tgid()
        || (require_stopped
            && (!matches!(tracee.phase, Phase::Stopped { .. }) || tracee.claim.is_some()))
    {
        return Err(PtraceError::NotFound);
    }
    thread_by_tid(&graph, tid).ok_or(PtraceError::NotFound)?;
    Ok(PtraceTarget {
        pid: tracee.pid.clone(),
    })
}

/// 在真实 stopped relation 内执行请求；寄存器 transaction 不跨 user-copy/睡眠。
///
/// # Parameters
/// - `selected`: user-copy 前捕获的原 attachment token。
/// - `request`: ABI adapter 已验证的请求。
///
/// # Returns
/// 读取的 word、regset 长度或零。
///
/// # Errors
/// 非当前 tracer/目标未停返回 NotFound；不支持的 regset/坏状态返回 Io/Invalid。
pub(crate) fn ptrace_request(
    selected: &PtraceTarget,
    request: PtraceRequest<'_>,
) -> Result<usize, PtraceError> {
    let caller = current_task().expect("ptrace request requires caller");
    let tid = selected.pid.identity().key();
    let mut graph = PROCESS_TABLE.graph.lock();
    let tracee = graph
        .tracees
        .get(&tid)
        .filter(|tracee| Arc::ptr_eq(&tracee.pid, &selected.pid))
        .ok_or(PtraceError::NotFound)?;
    if tracee.tracer_group != caller.tgid() {
        return Err(PtraceError::NotFound);
    }
    let target = thread_by_tid(&graph, tid).ok_or(PtraceError::NotFound)?.1;
    if matches!(request, PtraceRequest::Kill) {
        drop(graph);
        if !super::signal::send_kernel_process_signal(
            target.tgid(),
            SIGKILL,
            PendingSignal::kernel(),
        ) {
            return Err(PtraceError::NotFound);
        }
        return Ok(0);
    }
    if !matches!(tracee.phase, Phase::Stopped { .. }) || tracee.claim.is_some() {
        return Err(PtraceError::NotFound);
    }
    match request {
        PtraceRequest::GetRegisters(note, bytes) => target.ptrace_registers(note, bytes, false),
        PtraceRequest::SetRegisters(note, bytes) => {
            let mut buffer = [0u8; 272];
            if bytes.len() > buffer.len() {
                return Err(PtraceError::Invalid);
            }
            buffer[..bytes.len()].copy_from_slice(bytes);
            target.ptrace_registers(note, &mut buffer[..bytes.len()], true)
        }
        PtraceRequest::GetSignalInfo(bytes) => {
            let Phase::Stopped {
                info_signal, info, ..
            } = tracee.phase
            else {
                unreachable!()
            };
            *bytes = info.encode(info_signal);
            Ok(0)
        }
        PtraceRequest::SetSignalInfo { signal, info } => {
            let tracee = graph.tracees.get_mut(&tid).unwrap();
            let Phase::Stopped {
                info: old,
                info_signal,
                ..
            } = &mut tracee.phase
            else {
                unreachable!()
            };
            *old = info;
            *info_signal = signal;
            Ok(0)
        }
        PtraceRequest::Resume { signal, detach } => {
            if signal > 64 {
                return Err(PtraceError::Io);
            }
            let Phase::Stopped {
                info_signal,
                mut info,
                ..
            } = tracee.phase
            else {
                unreachable!()
            };
            // si_signo 可由 SETSIGINFO 独立修改；只有注入号与它不同，才按 Linux 替换为 tracer 的 SI_USER。
            if signal != 0 && signal != info_signal {
                let tracer = thread_by_tid(&graph, tracee.tracer)
                    .ok_or(PtraceError::NotFound)?
                    .1;
                let pid = tracer.tid_identity().number_in(&target.pid_namespace());
                let uid = target
                    .user_namespace()
                    .display_id(true, tracer.credential_id(true, false));
                info = PendingSignal::process_directed(pid as usize).with_identity(pid, uid, false);
            }
            let tracee = graph.tracees.get_mut(&tid).unwrap();
            target.set_ptrace_resume(signal, info);
            tracee.phase = Phase::Running;
            let retired = detach.then(|| unlink_tracee(&mut graph, tid));
            crate::execution::task::processor::continue_stopped_task(target.clone());
            drop(graph);
            drop(retired);
            Ok(0)
        }
        PtraceRequest::ReadWord(address) => {
            drop(graph);
            let mut bytes = [0u8; 8];
            target
                .ptrace_word(address, &mut bytes, false)
                .map_err(|_| PtraceError::Io)?;
            Ok(usize::from_ne_bytes(bytes))
        }
        PtraceRequest::WriteWord(address, value) => {
            drop(graph);
            target
                .ptrace_word(address, &mut value.to_ne_bytes(), true)
                .map_err(|_| PtraceError::Io)?;
            Ok(0)
        }
        PtraceRequest::Kill => unreachable!(),
    }
}

/// wait 扫描真实 tracing relation；ptrace-stop 对 wait4 不要求 WUNTRACED。
/// stop/exit 的 claim 与普通 child 共用 graph 事务，防止同组 waiter 重复消费。
pub(super) fn find_event(
    graph: &mut ProcessGraph,
    parent: usize,
    claimant: usize,
    selector: wait_child::ResolvedChildSelector,
    exited: bool,
    stopped: bool,
    creator_only: bool,
) -> (bool, Option<wait_child::ChildExit>) {
    let mut found = false;
    let mut chosen = None;
    let mut cursor = 0;
    loop {
        let tid = match selector {
            wait_child::ResolvedChildSelector::Process(tid) if cursor == 0 => Some(tid),
            wait_child::ResolvedChildSelector::Process(_) => break,
            _ => graph
                .nodes
                .get(&parent)
                .and_then(|node| node.ptracees.successor(&cursor))
                .map(|(&tid, ())| tid),
        };
        let Some(tid) = tid else {
            break;
        };
        let Some(tracee) = graph.tracees.get(&tid) else {
            break;
        };
        cursor = tid;
        if tracee.tracer_group != parent || (creator_only && tracee.tracer != claimant) {
            continue;
        }
        let matches = match selector {
            wait_child::ResolvedChildSelector::All => true,
            wait_child::ResolvedChildSelector::Process(pid) => tid == pid,
            wait_child::ResolvedChildSelector::Group(group) => graph
                .nodes
                .get(&tracee.tgid)
                .is_some_and(|node| node.process_group == group),
        };
        if !matches {
            continue;
        }
        found = true;
        if tracee.claim.is_some() {
            continue;
        }
        let (status, kind) = match tracee.phase {
            Phase::Stopped {
                signal,
                reported: false,
                ..
            } if stopped => (
                ((signal as i32) << 8) | 0x7f,
                wait_child::ChildStatusKind::TracedStop,
            ),
            Phase::Exited(status)
                if exited
                    && (tid != tracee.tgid
                        || graph.nodes.get(&tid).is_some_and(|node| {
                            matches!(
                                node.state,
                                ProcessState::Exited(
                                    _,
                                    ExitRetirement::Waitable | ExitRetirement::Traced
                                )
                            )
                        })) =>
            {
                (status, wait_child::ChildStatusKind::TracedExit)
            }
            _ => continue,
        };
        chosen = Some(wait_child::ChildExit {
            pid: tid,
            status,
            kind,
            claimant,
            trace: Some(TraceClaim {
                pid: tracee.pid.clone(),
                uid: tracee.uid,
            }),
        });
        break;
    }
    if let Some(record) = &chosen {
        graph.tracees.get_mut(&record.pid).unwrap().claim = Some(claimant);
    }
    (found, chosen)
}

pub(super) fn status_info(
    record: &wait_child::ChildExit,
    task: &TaskControlBlock,
) -> (u32, u32, i32, i32) {
    let tracee = record.trace.as_ref().expect("traced event owns snapshot");
    let (code, status) = if record.kind == wait_child::ChildStatusKind::TracedStop {
        (4, (record.status >> 8) & 0xff)
    } else if record.status & 0x7f == 0 {
        (1, record.status >> 8)
    } else {
        (2, record.status & 0x7f)
    };
    (
        tracee.pid.identity().number_in(&task.pid_namespace()),
        task.user_namespace().display_id(true, tracee.uid),
        code,
        status,
    )
}

pub(super) fn finish_claim(record: wait_child::ChildExit, consume: bool) {
    let (retired, processes, publish) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let claim = record.trace.as_ref().expect("traced wait owns snapshot");
        let Some(tracee) = graph
            .tracees
            .get_mut(&record.pid)
            .filter(|tracee| Arc::ptr_eq(&tracee.pid, &claim.pid))
        else {
            return;
        };
        assert_eq!(tracee.claim.take(), Some(record.claimant));
        let group = tracee.tracer_group;
        let tgid = tracee.tgid;
        if consume && let Phase::Stopped { reported, .. } = &mut tracee.phase {
            *reported = true;
        }
        let retired = (consume && record.kind == wait_child::ChildStatusKind::TracedExit)
            .then(|| unlink_tracee(&mut graph, record.pid));
        let mut processes = super::process_retirement::RetiredProcesses::default();
        let mut publish = None;
        if retired.is_some() && record.pid == tgid {
            let node = graph
                .nodes
                .get_mut(&tgid)
                .expect("traced leader owns final process");
            let ProcessState::Exited(status, ExitRetirement::Traced) = node.state else {
                panic!("trace exit must follow final retirement");
            };
            if node.parent == Some(group) {
                let usage = node.process.final_resource_usage();
                if let Some(parent) = graph.nodes.get(&group) {
                    parent.process.absorb_reaped_child(&usage);
                }
                processes.retire(&mut graph, tgid);
            } else {
                node.state = ProcessState::Exited(status, ExitRetirement::Retiring);
                publish = Some(tgid);
            }
        }
        wake_tracer(&mut graph, group);
        (retired, processes, publish)
    };
    drop(retired);
    drop(processes);
    if let Some(tgid) = publish {
        super::process_exit::complete_process_exit(tgid);
    }
}

/// Thread 的真实退出清理完成后发表 trace exit；tracer 退出解除它拥有的全部 relation。
/// 只保留 PID identity 与 status，不让 zombie tracee 保活 TCB/MM/files。
pub(super) fn thread_exited(task: &TaskControlBlock, status: i32) {
    let mut cursor = 0;
    loop {
        let (retired, resume, publish) = {
            let mut graph = PROCESS_TABLE.graph.lock();
            let next = graph.nodes.get(&task.tgid()).and_then(|node| {
                node.ptracees
                    .iter_after(&cursor)
                    .find(|(tid, _)| graph.tracees[tid].tracer == task.tid())
                    .map(|(&tid, ())| tid)
            });
            let Some(tid) = next else {
                break;
            };
            cursor = tid;
            let resume = thread_by_tid(&graph, tid).map(|(_, task)| task);
            let retired = unlink_tracee(&mut graph, tid);
            if let Some(target) = &resume {
                if retired
                    .as_ref()
                    .is_some_and(|tracee| matches!(tracee.phase, Phase::Stopped { .. }))
                {
                    target.set_ptrace_resume(0, PendingSignal::kernel());
                }
                crate::execution::task::processor::continue_stopped_task(target.clone());
            }
            if let Some(parent) = retired
                .as_ref()
                .and_then(|tracee| graph.nodes.get(&tracee.tgid))
                .and_then(|node| node.parent)
            {
                wake_tracer(&mut graph, parent);
            }
            let publish = if retired.as_ref().is_some_and(|tracee| tracee.tgid == tid) {
                graph.nodes.get_mut(&tid).and_then(|node| match node.state {
                    ProcessState::Exited(status, ExitRetirement::Traced) => {
                        node.state = ProcessState::Exited(status, ExitRetirement::Retiring);
                        Some(tid)
                    }
                    _ => None,
                })
            } else {
                None
            };
            (retired, resume, publish)
        };
        drop(retired);
        drop(resume);
        if let Some(tgid) = publish {
            super::process_exit::complete_process_exit(tgid);
        }
    }
    let mut graph = PROCESS_TABLE.graph.lock();
    if let Some(tracee) = graph.tracees.get_mut(&task.tid()) {
        tracee.uid = task.credential_id(true, false);
        tracee.phase = Phase::Exited(status);
        let group = tracee.tracer_group;
        wake_tracer(&mut graph, group);
    }
    let notice = (task.tid() != task.tgid())
        .then(|| {
            graph
                .tracees
                .get(&task.tid())
                .and_then(|tracee| exit_notice(&graph, tracee, status))
        })
        .flatten();
    drop(graph);
    if let Some((group, info)) = notice {
        super::signal::send_kernel_process_signal(group, syscall_abi::signal::SIGCHLD, info);
    }
}

/// exec 的新 context 已发布；传统 TRACEME/ATTACH 在返回用户态前产生 SIGTRAP。
pub(in crate::execution::task) fn exec_completed(task: &TaskControlBlock) {
    let graph = PROCESS_TABLE.graph.lock();
    if graph.tracees.contains_key(&task.tid()) {
        task.queue_ptrace_signal(syscall_abi::signal::SIGTRAP);
    }
}

/// 未具备目标 UserNS SYS_PTRACE 的 tracer 不能借被跟踪的 set-id/file-capability exec 提权。
pub(in crate::execution::task) fn restrict_exec(task: &TaskControlBlock) -> bool {
    let graph = PROCESS_TABLE.graph.lock();
    graph.tracees.get(&task.tid()).is_some_and(|tracee| {
        thread_by_tid(&graph, tracee.tracer).is_none_or(|(_, tracer)| {
            !tracer.namespace_authority().capable(
                &task.user_namespace(),
                syscall_abi::capability::Capability::SYS_PTRACE,
            )
        })
    })
}

/// leader 的实际 retirement 完成后，先把 exit 交给 tracer；real parent 尚不可 reap。
pub(super) fn defer_process_exit(tgid: usize) -> bool {
    let (source, notice) = {
        let mut graph = PROCESS_TABLE.graph.lock();
        let Some(tracee) = graph.tracees.get(&tgid) else {
            return false;
        };
        let group = tracee.tracer_group;
        let node = graph
            .nodes
            .get_mut(&tgid)
            .expect("traced process remains indexed");
        let ProcessState::Exited(status, ExitRetirement::Retiring) = node.state else {
            panic!("ptrace exit precedes retirement");
        };
        node.state = ProcessState::Exited(status, ExitRetirement::Traced);
        let source = node.pidfd_source.upgrade();
        wake_tracer(&mut graph, group);
        let notice = exit_notice(&graph, &graph.tracees[&tgid], status.wait_status());
        (source, notice)
    };
    if let Some(source) = source {
        source.notify();
    }
    // SIGCHLD 真实 generation 在 graph 外准备 metadata；parent notification 延迟到 tracer 消费。
    if let Some((group, info)) = notice {
        super::signal::send_kernel_process_signal(group, syscall_abi::signal::SIGCHLD, info);
    }
    true
}

/// 已持 exec credential guard 时为非 leader tracee 的最终身份准备一个真实 PID link。
pub(in crate::execution::task) fn prepare_exec_link(
    task: &TaskControlBlock,
) -> Result<Option<Arc<PidRegistration>>, crate::memory::ElfLoadError> {
    if task.tid() == task.tgid() || !PROCESS_TABLE.graph.lock().tracees.contains_key(&task.tid()) {
        return Ok(None);
    }
    let link = PidLinkSlot::try_new().map_err(|_| crate::memory::ElfLoadError::OutOfMemory)?;
    let registration = PROCESS_TABLE.graph.lock().nodes[&task.tgid()]
        .pid
        .clone_in(link);
    Arc::try_new(registration)
        .map(Some)
        .map_err(|_| crate::memory::ElfLoadError::OutOfMemory)
}

/// 与 exec 身份提升同一 graph transaction 移交关系；旧 association 在 guard 外退休。
pub(super) fn promote_exec(
    graph: &mut ProcessGraph,
    task: &TaskControlBlock,
    old_tid: usize,
    link: Option<Arc<PidRegistration>>,
) -> (Option<Tracee>, Option<Arc<PidRegistration>>) {
    if old_tid == task.tid() {
        assert!(link.is_none());
        return (None, None);
    }
    let mut cursor = 0;
    while let Some((&tid, _)) = graph.tracees.successor(&cursor) {
        cursor = tid;
        let tracee = graph.tracees.get_mut(&tid).unwrap();
        if tracee.tracer == old_tid {
            tracee.tracer = task.tid();
        }
    }
    let retired_leader = unlink_tracee(graph, task.tid());
    let retired_pid = if let Some(mut entry) = graph.tracees.take_entry(&old_tid) {
        let registration = link.expect("exec trace link prepared");
        let retired = core::mem::replace(&mut entry.value_mut().pid, registration);
        let group = entry.value().tracer_group;
        let memberships = &mut graph
            .nodes
            .get_mut(&group)
            .expect("live tracer group")
            .ptracees;
        let mut membership = memberships.take_entry(&old_tid).expect("trace membership");
        membership.set_key(task.tid());
        memberships.commit_vacant(membership);
        entry.set_key(task.tid());
        graph.tracees.commit_vacant(entry);
        Some(retired)
    } else {
        assert!(link.is_none());
        None
    };
    (retired_leader, retired_pid)
}

/// SIGCONT 只能解除 job-control stop，不能越过独立 ptrace-stop。
pub(super) fn is_stopped(graph: &ProcessGraph, tid: usize) -> bool {
    graph.tracees.get(&tid).is_some_and(|tracee| {
        matches!(tracee.phase, Phase::Stopping { .. } | Phase::Stopped { .. })
    })
}

// 移除唯一 relation 的同时注销其 membership；所有最终 Drop 由 caller 在 graph 外执行。
fn unlink_tracee(graph: &mut ProcessGraph, tid: usize) -> Option<Tracee> {
    let tracee = graph.tracees.remove(&tid)?;
    graph
        .nodes
        .get_mut(&tracee.tracer_group)
        .expect("trace relation retains live tracer group")
        .ptracees
        .remove(&tid)
        .expect("trace membership exists");
    Some(tracee)
}

fn exit_notice(
    graph: &ProcessGraph,
    tracee: &Tracee,
    status: i32,
) -> Option<(usize, PendingSignal)> {
    let tracer = thread_by_tid(graph, tracee.tracer)?.1;
    let pid = tracee.pid.identity().number_in(&tracer.pid_namespace());
    let uid = tracer.user_namespace().display_id(true, tracee.uid);
    let info = if status & 0x7f == 0 {
        PendingSignal::child_exited(pid as usize, status >> 8)
    } else {
        PendingSignal::child_killed(pid as usize, (status & 0x7f) as usize)
    };
    Some((tracee.tracer_group, info.with_identity(pid, uid, false)))
}
