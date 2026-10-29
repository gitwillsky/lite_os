use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildStatusKind {
    Exited,
    Stopped,
    Continued,
    TracedStop,
    TracedExit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ChildWaitClaim {
    pub(super) waiter: usize,
    pub(super) kind: ChildStatusKind,
}

/// 取走 parent Process 的全部 child-event waiter，由调用者在 graph 锁外唤醒。
///
/// # Parameters
///
/// - `node`: process graph 内的 parent node。
///
/// # Returns
///
/// 以全局 TID 去重的 waiter owner；各 Thread 醒来后按自己的 selector 重新检查。
pub(super) fn take_child_waiters(
    node: &mut ProcessNode,
) -> FallibleMap<usize, Arc<TaskControlBlock>> {
    core::mem::take(&mut node.child_waiters)
}

#[derive(Debug, Clone)]
pub(crate) struct ChildExit {
    pub(crate) pid: usize,
    pub(crate) status: i32,
    pub(super) kind: ChildStatusKind,
    pub(super) claimant: usize,
    pub(super) trace: Option<super::ptrace::TraceClaim>,
}

/// wait4 在 task layer 的精确结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitChildError {
    NoChild,
    InvalidSelector,
    Interrupted,
    OutOfMemory,
}

/// Linux child class 按实际 exit_signal 分类，__WALL 覆盖 __WCLONE。
#[derive(Clone, Copy)]
pub(crate) enum ChildWaitClass {
    Ordinary,
    Clone,
    All,
}

/// 已解码的等待选择；不保存副本 child relation 或 raw syscall flags。
#[derive(Clone, Copy)]
pub(crate) struct ChildWaitFilter {
    pub(crate) class: ChildWaitClass,
    pub(crate) creator_only: bool,
    pub(crate) ptrace_stops: bool,
}

impl ChildWaitFilter {
    fn eligible(self, node: &ProcessNode, claimant: usize) -> bool {
        (!self.creator_only || node.parent_thread == Some(claimant))
            && match self.class {
                ChildWaitClass::Ordinary => node.exit_signal == syscall_abi::signal::SIGCHLD as u8,
                ChildWaitClass::Clone => node.exit_signal != syscall_abi::signal::SIGCHLD as u8,
                ChildWaitClass::All => true,
            }
    }
}

/// 调用 view 中的选择或实际 PIDFD identity；解析后等待只持不可复用 graph key。
pub(crate) enum ChildWaitSelector {
    All,
    Process(usize),
    Group(usize),
    Pid(Arc<crate::namespace::PidIdentity>),
}
#[derive(Clone, Copy)]
pub(super) enum ResolvedChildSelector {
    All,
    Process(usize),
    Group(usize),
}
impl ResolvedChildSelector {
    fn resolve(
        selector: ChildWaitSelector,
        task: &TaskControlBlock,
    ) -> Result<Self, WaitChildError> {
        Ok(match selector {
            ChildWaitSelector::All => Self::All,
            ChildWaitSelector::Process(number) => {
                Self::Process(task.resolve_pid(number).ok_or(WaitChildError::NoChild)?)
            }
            ChildWaitSelector::Group(0) => {
                Self::Group(PROCESS_TABLE.graph.lock().nodes[&task.tgid()].process_group)
            }
            ChildWaitSelector::Group(number) => {
                Self::Group(task.resolve_pid(number).ok_or(WaitChildError::NoChild)?)
            }
            ChildWaitSelector::Pid(identity) => Self::Process(identity.key()),
        })
    }
    fn matches(self, child: usize, node: &ProcessNode) -> bool {
        match self {
            Self::All => true,
            Self::Process(pid) => child == pid,
            Self::Group(group) => node.process_group == group,
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "同一次原子 publication 的独立领域输入，参数语义与所有权已逐项说明"
)]
fn find_waitable_child(
    graph: &mut ProcessGraph,
    parent: usize,
    claimant: usize,
    selector: ResolvedChildSelector,
    include_exited: bool,
    include_stopped: bool,
    include_continued: bool,
    filter: ChildWaitFilter,
) -> Result<Option<ChildExit>, WaitChildError> {
    let (mut has_child, traced) = super::ptrace::find_event(
        graph,
        parent,
        claimant,
        selector,
        include_exited,
        filter.ptrace_stops,
        filter.creator_only,
    );
    if traced.is_some() {
        return Ok(traced);
    }
    let mut selected = None;
    let mut cursor = 0;
    loop {
        let pid = match selector {
            ResolvedChildSelector::Process(pid) if cursor == 0 => Some(pid),
            ResolvedChildSelector::Process(_) => break,
            _ => graph
                .nodes
                .get(&parent)
                .and_then(|node| node.children.successor(&cursor))
                .map(|(&pid, _)| pid),
        };
        let Some(pid) = pid else {
            break;
        };
        cursor = pid;
        let Some(node) = graph.nodes.get(&pid) else {
            // Positive waitpid selector is a caller-supplied PID, not a parent-index node. A
            // competing parent Thread may already have consumed that child's unique exit claim;
            // Linux requires the loser to observe ECHILD, not an index-corruption panic.
            if matches!(selector, ResolvedChildSelector::Process(_)) {
                break;
            }
            panic!("parent child index references missing process");
        };
        if node.parent != Some(parent)
            || !filter.eligible(node, claimant)
            || !selector.matches(pid, node)
        {
            if matches!(selector, ResolvedChildSelector::Process(_)) {
                break;
            }
            continue;
        }
        // real parent 仍拥有 child，但必须等 tracer 消费最终 exit，不能提前返回 ECHILD。
        if graph.tracees.contains_key(&pid) {
            has_child = true;
            continue;
        }
        // 已完成退出的 zombie 不能再产生 stop/continue；native WEXITED 缺失时不计作可等待 child。
        if !include_exited
            && matches!(
                node.state,
                ProcessState::Exited(_, ExitRetirement::Waitable | ExitRetirement::AfterClaim)
            )
        {
            continue;
        }
        has_child = true;
        if node.child_wait_claim.is_some() {
            continue;
        }
        if include_exited && let ProcessState::Exited(status, ExitRetirement::Waitable) = node.state
        {
            selected = Some(ChildExit {
                pid,
                status: status.wait_status(),
                kind: ChildStatusKind::Exited,
                claimant,
                trace: None,
            });
            break;
        }
        if include_stopped && let Some(signal) = node.child_events.stopped {
            selected = Some(ChildExit {
                pid,
                status: ((signal as i32) << 8) | 0x7f,
                kind: ChildStatusKind::Stopped,
                claimant,
                trace: None,
            });
            break;
        }
        if include_continued && node.child_events.continued {
            selected = Some(ChildExit {
                pid,
                status: 0xffff,
                kind: ChildStatusKind::Continued,
                claimant,
                trace: None,
            });
            break;
        }
    }
    let Some(record) = selected else {
        return if has_child {
            Ok(None)
        } else {
            Err(WaitChildError::NoChild)
        };
    };
    let node = graph
        .nodes
        .get_mut(&record.pid)
        .expect("selected child disappeared while graph is locked");
    assert!(
        node.child_wait_claim
            .replace(ChildWaitClaim {
                waiter: claimant,
                kind: record.kind,
            })
            .is_none(),
        "child event claimed twice"
    );
    Ok(Some(record))
}

/// 等待直接 child 的 exit、stopped 或 continued 状态。
///
/// # Parameters
///
/// - `selector`: `>0` 为 PID，`-1` 为任一 child，`0`/`<-1` 为 process group。
/// - `nohang`: 无可消费 record 时是否立即返回。
/// - `include_stopped`: 是否消费尚未报告的 job-control stop。
/// - `include_continued`: 是否消费尚未报告的 continue。
/// - `filter`: native clone class 与仅 calling Thread 的 child 选择。
///
/// # Returns
///
/// child record、WNOHANG 的 None，或 selector/child/interruption 错误。
pub(crate) fn wait_child(
    selector: isize,
    nohang: bool,
    include_stopped: bool,
    include_continued: bool,
    filter: ChildWaitFilter,
) -> Result<Option<ChildExit>, WaitChildError> {
    let selector = match selector {
        -1 => ChildWaitSelector::All,
        value if value > 0 => ChildWaitSelector::Process(value as usize),
        value => ChildWaitSelector::Group(
            value.checked_neg().ok_or(WaitChildError::InvalidSelector)? as usize,
        ),
    };
    wait_child_events(
        selector,
        nohang,
        true,
        include_stopped,
        include_continued,
        filter,
    )
}

/// waitid 和 legacy wait 使用同一实际 child-claim publication；PIDFD 直接传稳定 identity。
/// process group 在调用时冻结，阻塞期间 parent setpgid 不会改变此次等待的 selection。
pub(crate) fn wait_child_events(
    selector: ChildWaitSelector,
    nohang: bool,
    include_exited: bool,
    include_stopped: bool,
    include_continued: bool,
    filter: ChildWaitFilter,
) -> Result<Option<ChildExit>, WaitChildError> {
    let task = current_task().expect("wait requires current Thread");
    let selector = ResolvedChildSelector::resolve(selector, &task)?;
    wait_selected_child(
        &task,
        selector,
        nohang,
        include_exited,
        include_stopped,
        include_continued,
        filter,
    )
}

fn wait_selected_child(
    task: &Arc<TaskControlBlock>,
    selector: ResolvedChildSelector,
    nohang: bool,
    include_exited: bool,
    include_stopped: bool,
    include_continued: bool,
    filter: ChildWaitFilter,
) -> Result<Option<ChildExit>, WaitChildError> {
    let parent = task.tgid();
    loop {
        // Storage is prepared outside the graph owner. Event/nohang/signal decisions remain
        // successful on OOM; only publishing a blocking waiter requires this allocation.
        let waiter = FallibleMap::<usize, Arc<TaskControlBlock>>::try_reserve_node();
        let mut graph = PROCESS_TABLE.graph.lock();
        let record = find_waitable_child(
            &mut graph,
            parent,
            task.tid(),
            selector,
            include_exited,
            include_stopped,
            include_continued,
            filter,
        )?;
        match super::wait_publication::child_wait_publication(
            record.is_some(),
            nohang,
            task.has_deliverable_signal(),
            waiter.is_ok(),
        ) {
            super::wait_publication::ChildWaitPublication::ConsumeEvent => return Ok(record),
            super::wait_publication::ChildWaitPublication::ReturnNoHang => return Ok(None),
            super::wait_publication::ChildWaitPublication::Interrupted => {
                return Err(WaitChildError::Interrupted);
            }
            super::wait_publication::ChildWaitPublication::OutOfMemory => {
                return Err(WaitChildError::OutOfMemory);
            }
            super::wait_publication::ChildWaitPublication::Publish => {}
        }

        // graph lock 覆盖 child 复查与 waiter 发布；exit/job event 使用同一 owner，因此不会丢唤醒。
        let waiter = waiter
            .expect("publication decision requires prepared child waiter")
            .fill(task.tid(), task.clone());
        let prepared =
            super::context_switch::prepare_current_block(task, graph, move |graph, _| {
                let parent_node = graph
                    .nodes
                    .get_mut(&parent)
                    .expect("waiting parent missing from process graph");
                parent_node.child_waiters.commit_vacant(waiter);
                WaitMembership::Child
            });
        match prepared.suspend() {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(WaitChildError::Interrupted),
            WaitResult::TimedOut => panic!("child waiter cannot time out"),
            WaitResult::OutOfMemory => unreachable!("child wait has no indexed allocation"),
        }
    }
}

/// Active claim 下捕获 native waitid 字段；消费/保留 claim 后不再借已可回收的 Node。
pub(crate) fn child_status_info(record: &ChildExit) -> (u32, u32, i32, i32) {
    let task = current_task().expect("child info requires current waiter");
    let graph = PROCESS_TABLE.graph.lock();
    if matches!(
        record.kind,
        ChildStatusKind::TracedStop | ChildStatusKind::TracedExit
    ) {
        return super::ptrace::status_info(record, &task);
    }
    let node = graph
        .nodes
        .get(&record.pid)
        .expect("active claim owns child status");
    let pid = node.pid.identity().number_in(&task.pid_namespace());
    let uid = task.user_namespace().display_id(
        true,
        super::super::model::leader_signal_uid(&node.leader_credentials),
    );
    let (code, status) = match record.kind {
        ChildStatusKind::Exited => {
            if record.status & 0x7f == 0 {
                (1, record.status >> 8)
            } else {
                (2, record.status & 0x7f)
            }
        }
        ChildStatusKind::Stopped => (5, (record.status >> 8) & 0xff),
        ChildStatusKind::Continued => (6, syscall_abi::signal::SIGCONT as i32),
        ChildStatusKind::TracedStop | ChildStatusKind::TracedExit => unreachable!(),
    };
    (pid, uid, code, status)
}

mod claim;
pub(crate) use claim::{consume_child_status, release_child_status};

/// `RUSAGE_CHILDREN`：被 wait 回收的 child 退出后使用量不再变化，回收时一次性并入 parent。
///
/// 只在 `consume_child_status` 的 graph 锁内调用；自动回收与 `WNOWAIT` 不经过这里。
fn absorb_reaped_usage(graph: &ProcessGraph, child: usize, parent: Option<usize>) {
    let usage = graph.nodes[&child].process.final_resource_usage();
    if let Some(parent) = parent.and_then(|pid| graph.nodes.get(&pid)) {
        parent.process.absorb_reaped_child(&usage);
    }
}

/// `wait4`/`waitid` 的 rusage：child 自身加上它已回收的后代（Linux `RUSAGE_BOTH`）。
///
/// Active claim pin 保证 child Node 尚未 reap；必须在 `consume_child_status` 之前读取。
pub(crate) fn child_resource_usage(record: &ChildExit) -> crate::execution::task::ResourceUsage {
    if matches!(
        record.kind,
        ChildStatusKind::TracedStop | ChildStatusKind::TracedExit
    ) {
        return crate::execution::task::ResourceUsage::default();
    }
    PROCESS_TABLE
        .graph
        .lock()
        .nodes
        .get(&record.pid)
        .expect("active child claim owns exit record")
        .process
        .final_resource_usage()
}

/// Active claim pin 保证 child Node 尚未 reap；copyout 前捕获 calling view 的标准 PID。
pub(crate) fn child_status_pid(record: &ChildExit) -> usize {
    let namespace = current_task()
        .expect("child status requires current waiter")
        .pid_namespace();
    let graph = PROCESS_TABLE.graph.lock();
    if matches!(
        record.kind,
        ChildStatusKind::TracedStop | ChildStatusKind::TracedExit
    ) {
        return record
            .trace
            .as_ref()
            .expect("trace event snapshot")
            .pid
            .identity()
            .number_in(&namespace) as usize;
    }
    graph
        .nodes
        .get(&record.pid)
        .expect("active child claim owns exit record")
        .pid
        .identity()
        .number_in(&namespace) as usize
}
