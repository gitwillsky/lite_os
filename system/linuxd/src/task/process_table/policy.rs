use super::thread_selector::scheduler_thread;
use super::{PROCESS_TABLE, ProcessGraph, ProcessNode, ProcessState};
use crate::task::{
    TaskControlBlock, current_task, model::RLIMIT_NICE, processor::request_task_reschedule,
};
use alloc::{sync::Arc, vec::Vec};

const SCHED_OTHER: i32 = 0;
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;

/// legacy Linux scheduler policy 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerPolicyRequest {
    /// 查询 policy，不修改目标。
    Query,
    /// 保留 policy 与 reset-on-fork，只校验并设置 legacy priority。
    SetParameters {
        /// Linux `struct sched_param.sched_priority`。
        priority: i32,
    },
    /// 原子替换 legacy policy、reset-on-fork 与 priority。
    Replace {
        /// Linux legacy scheduler policy 与可选 reset-on-fork bit。
        policy: i32,
        /// Linux `struct sched_param.sched_priority`。
        priority: i32,
    },
}

/// legacy Linux scheduler policy operation 的领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerPolicyError {
    Access,
    Invalid,
    NotFound,
    OutOfMemory,
    Permission,
    Busy,
    Unsupported,
}

/// Linux get/setpriority 的 task collection selector。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerNiceSelector {
    /// 零选择 caller，非零选择calling PID namespace 的 TID。
    Process(u32),
    /// 零选择 caller process group，非零选择 PGID。
    Group(u32),
    /// 零选择 caller real UID，非零选择给定 UID。
    User(u32),
}

// 单次 selector 的实际 graph iterator；group 从 membership owner 迭代，不扫描无关进程。
enum NiceProcesses<'a> {
    All(fallible_tree::Iter<'a, usize, ProcessNode>),
    Group {
        graph: &'a ProcessGraph,
        members: fallible_tree::Iter<'a, usize, ()>,
    },
}
impl<'a> Iterator for NiceProcesses<'a> {
    type Item = &'a ProcessNode;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::All(nodes) => nodes.next().map(|(_, node)| node),
            Self::Group { graph, members } => members.next().map(|(key, ())| {
                graph
                    .nodes
                    .get(key)
                    .expect("nice selector group member missing")
            }),
        }
    }
}

fn nice_targets(
    selector: SchedulerNiceSelector,
    caller: &Arc<TaskControlBlock>,
) -> Result<Vec<Arc<TaskControlBlock>>, SchedulerPolicyError> {
    if let SchedulerNiceSelector::Process(tid) = selector {
        let Some(target) = scheduler_thread(tid as usize, caller) else {
            return Ok(Vec::new());
        };
        let mut targets = Vec::new();
        targets
            .try_reserve_exact(1)
            .map_err(|_| SchedulerPolicyError::OutOfMemory)?;
        targets.push(target);
        return Ok(targets);
    }
    let view = caller.pid_namespace();
    let requested_group = match selector {
        SchedulerNiceSelector::Group(group) if group != 0 => {
            let Some(key) = caller.resolve_pid(group as usize) else {
                return Ok(Vec::new());
            };
            Some(key)
        }
        _ => None,
    };
    let mut candidates = Vec::new();
    loop {
        let graph = PROCESS_TABLE.graph.lock();
        let selected_group = match selector {
            SchedulerNiceSelector::Group(0) => {
                let Some(node) = graph.nodes.get(&caller.tgid()) else {
                    return Ok(Vec::new());
                };
                Some(node.process_group)
            }
            SchedulerNiceSelector::Group(_) => requested_group,
            SchedulerNiceSelector::Process(_) | SchedulerNiceSelector::User(_) => None,
        };
        if selected_group.is_some_and(|key| !graph.groups.contains_key(&key)) {
            return Ok(Vec::new());
        }
        let nodes = || match selected_group {
            Some(key) => NiceProcesses::Group {
                graph: &graph,
                members: graph
                    .groups
                    .get(&key)
                    .expect("caller/validated PGID registration")
                    .members
                    .iter(),
            },
            None => NiceProcesses::All(graph.nodes.iter()),
        };
        let required = nodes()
            .filter(|node| node.pid.identity().number_in(&view) != 0)
            .try_fold(0usize, |count, node| match &node.state {
                ProcessState::Live(threads) => count.checked_add(threads.len()).ok_or(()),
                ProcessState::Exited(..) => Ok(count),
            })
            .map_err(|()| SchedulerPolicyError::OutOfMemory)?;
        match super::snapshot_staging::snapshot_capacity(candidates.capacity(), required) {
            super::snapshot_staging::SnapshotCapacity::Retry { minimum } => {
                drop(graph);
                candidates.clear();
                candidates
                    .try_reserve_exact(minimum)
                    .map_err(|_| SchedulerPolicyError::OutOfMemory)?;
            }
            super::snapshot_staging::SnapshotCapacity::Capture => {
                candidates.clear();
                for node in nodes().filter(|node| node.pid.identity().number_in(&view) != 0) {
                    let ProcessState::Live(threads) = &node.state else {
                        continue;
                    };
                    candidates.extend(threads.values().cloned());
                }
                break;
            }
        }
    }
    let SchedulerNiceSelector::User(requested_uid) = selector else {
        return Ok(candidates);
    };
    let uid = if requested_uid == 0 {
        caller.credential_id(true, false)
    } else {
        let Some(uid) = caller.user_namespace().to_kernel(true, requested_uid) else {
            return Ok(Vec::new());
        };
        uid
    };
    candidates.retain(|target| target.credential_id(true, false) == uid);
    Ok(candidates)
}

/// 查询或替换 Linux selector 命中的 live Thread nice 值。
///
/// # Parameters
///
/// - `selector`: PROCESS/PGRP/USER collection selector；零值由 calling Thread 解析。
/// - `replacement`: `None` 查询集合中最小 nice；`Some` 按 Linux 范围钳制后逐目标设置。
///
/// # Returns
///
/// 查询返回最小 nice；设置成功返回规范化 nice。
///
/// # Errors
///
/// 空集合返回 `NotFound`；身份不匹配返回 `Permission`；提高优先级越过 RLIMIT_NICE 返回 `Access`。
pub(crate) fn scheduler_nice(
    selector: SchedulerNiceSelector,
    replacement: Option<i32>,
) -> Result<i32, SchedulerPolicyError> {
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    let targets = nice_targets(selector, &caller)?;
    let Some(requested) = replacement.map(|nice| nice.clamp(-20, 19)) else {
        return targets
            .iter()
            .map(|target| target.scheduling.policy.lock().nice(None))
            .min()
            .ok_or(SchedulerPolicyError::NotFound);
    };

    // 1. Linux 允许 collection set 部分成功；成功只清除初始 ESRCH，不覆盖此前错误。
    let mut result = Err(SchedulerPolicyError::NotFound);
    for target in targets {
        let Some(privileged) = caller.scheduler_privilege_for(&target) else {
            result = Err(SchedulerPolicyError::Permission);
            continue;
        };
        let mut policy = target.scheduling.policy.lock();
        let previous = policy.nice(None);
        // 2. 数值更小才是提高优先级；root 或目标 Process 的 RLIMIT_NICE soft 可授权。
        let limit = target
            .resource_limit(RLIMIT_NICE)
            .expect("RLIMIT_NICE must exist")
            .soft;
        if requested < previous && !privileged && (20 - requested) as u64 > limit {
            result = Err(SchedulerPolicyError::Access);
            continue;
        }
        let changed = requested != previous;
        if changed {
            policy.nice(Some(requested));
        }
        drop(policy);
        // 3. Running target 尽快结束使用旧权重快照的 slice；Ready/blocked target 无额外 membership。
        if changed {
            request_task_reschedule(&target);
        }
        if result == Err(SchedulerPolicyError::NotFound) {
            result = Ok(requested);
        }
    }
    result
}

/// 查询或替换一个 live Thread 的 Linux I/O priority policy。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数使用calling PID namespace 的 TID selector。
/// - `replacement`: None 查询，Some 替换已验证的 encoded priority。
///
/// # Returns
///
/// 当前或新 I/O priority。
///
/// # Errors
///
/// caller/目标不存在返回 NotFound；设置其他身份目标且无 root 权限返回 Permission。
pub(crate) fn scheduler_io_priority(
    tid: usize,
    replacement: Option<u16>,
) -> Result<u16, SchedulerPolicyError> {
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    let target = scheduler_thread(tid, &caller).ok_or(SchedulerPolicyError::NotFound)?;
    if replacement.is_some() && caller.scheduler_privilege_for(&target).is_none() {
        return Err(SchedulerPolicyError::Permission);
    }
    let mut policy = target.scheduling.policy.lock();
    let previous = policy.io_priority(replacement);
    Ok(replacement.unwrap_or(previous))
}

/// 查询或替换 live Thread 的 legacy Linux scheduler policy。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数使用 Linux calling PID namespace 的 TID selector。
/// - `request`: 查询、保留 policy 设置参数，或替换完整 legacy policy。
///
/// # Returns
///
/// 当前 `SCHED_OTHER`，并在 owner 设置时包含 `SCHED_RESET_ON_FORK`。
///
/// # Errors
///
/// 目标不存在返回 `NotFound`；policy/priority 不可表达返回 `Invalid`；权限不足返回 `Permission`。
pub(crate) fn scheduler_policy(
    tid: usize,
    request: SchedulerPolicyRequest,
) -> Result<i32, SchedulerPolicyError> {
    update_attributes(tid, |old| match request {
        SchedulerPolicyRequest::Query => Ok(None),
        SchedulerPolicyRequest::SetParameters { priority } => {
            if priority != 0 || old.policy != SCHED_OTHER as u32 {
                return Err(SchedulerPolicyError::Invalid);
            }
            Ok(Some(old))
        }
        SchedulerPolicyRequest::Replace { policy, priority } => {
            if priority != 0 || policy & !SCHED_RESET_ON_FORK != SCHED_OTHER {
                return Err(SchedulerPolicyError::Invalid);
            }
            Ok(Some(crate::task::model::SchedulingAttributes {
                flags: u64::from(policy & SCHED_RESET_ON_FORK != 0),
                nice: old.nice,
                ..Default::default()
            }))
        }
    })
    .map(|attributes| {
        attributes.policy as i32
            | if attributes.flags & 1 != 0 {
                SCHED_RESET_ON_FORK
            } else {
                0
            }
    })
}

/// 查询 live Thread 可观察的 scheduler 基础时间片。
///
/// # Parameters
///
/// - `tid`: 零选择 calling Thread；正数使用 Linux calling PID namespace 的 TID selector。
///
/// # Returns
///
/// timer owner 校准后的固定 `SCHED_OTHER` preemption quantum，单位纳秒。
///
/// # Errors
///
/// calling Thread 或目标不存在时返回 `NotFound`。
pub(crate) fn scheduler_rr_interval(tid: usize) -> Result<u64, SchedulerPolicyError> {
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    let target = scheduler_thread(tid, &caller).ok_or(SchedulerPolicyError::NotFound)?;
    Ok(if target.scheduling.policy.lock().deadline().is_some() {
        0
    } else {
        crate::time::scheduler_quantum_ns()
    })
}

/// 标准 sched_attr 的配置事务；Linux 权限/flags 在此，动态 CBS 状态和带宽只由原生调度器拥有。
/// 查询返回当前已提交 Linux 配置；替换失败保留旧配置。用户复制不在任何策略锁内。
/// # Errors
/// ESRCH/EPERM/EACCES/EINVAL、未开放扩展 Unsupported、原生准入容量不足 Busy。
pub(crate) fn scheduler_attributes(
    tid: usize,
    replacement: Option<crate::task::model::SchedulingAttributes>,
) -> Result<crate::task::model::SchedulingAttributes, SchedulerPolicyError> {
    update_attributes(tid, |_| Ok(replacement))
}

fn update_attributes(
    tid: usize,
    prepare: impl FnOnce(
        crate::task::model::SchedulingAttributes,
    ) -> Result<
        Option<crate::task::model::SchedulingAttributes>,
        SchedulerPolicyError,
    >,
) -> Result<crate::task::model::SchedulingAttributes, SchedulerPolicyError> {
    use crate::task::model::{DeadlineParameters, RLIMIT_NICE};
    use linux_abi::capability::Capability;
    use native_runtime::native_abi;
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    let target = scheduler_thread(tid, &caller).ok_or(SchedulerPolicyError::NotFound)?;
    let authority = caller.scheduler_privilege_for(&target);
    let mut policy = target.scheduling.policy.lock();
    let old = policy.attributes();
    let Some(mut requested) = prepare(old)? else {
        return Ok(old);
    };
    if requested.flags & !0x7f != 0 || requested.policy > i32::MAX as u32 {
        return Err(SchedulerPolicyError::Invalid);
    }
    // RECLAIM/OVERRUN/util-clamp 不伪装支持；KEEP_POLICY/PARAMS 在真实目标快照上解析。
    if requested.flags & (2 | 4 | 32 | 64) != 0 {
        return Err(SchedulerPolicyError::Unsupported);
    }
    if requested.flags & 8 != 0 {
        requested.policy = old.policy;
    }
    if requested.flags & 16 != 0 {
        requested.nice = old.nice;
        requested.priority = old.priority;
        requested.runtime = old.runtime;
        requested.deadline = old.deadline;
        requested.period = old.period;
    }
    requested.flags &= 1;
    if requested.priority != 0 || !matches!(requested.policy, 0 | 6) {
        return Err(SchedulerPolicyError::Invalid);
    }
    if requested.policy == 6 {
        if requested.period == 0 {
            requested.period = requested.deadline;
        }
        if requested.runtime < 1024
            || requested.runtime > requested.deadline
            || requested.deadline > requested.period
            || !(100_000..=4_194_304_000).contains(&requested.period)
        {
            return Err(SchedulerPolicyError::Invalid);
        }
    }
    let privileged = authority.ok_or(SchedulerPolicyError::Permission)?;
    let deadline = if requested.policy == 6 {
        if !caller.has_initial_capability(Capability::SYS_NICE) {
            return Err(SchedulerPolicyError::Permission);
        }
        // 一个原生分区就是已准入 CPU；Linux root domain 为全部 online CPU，避免掩码缩小破坏承诺。
        if target.scheduling.state.lock().cpu_affinity.effective_bits()
            != crate::cpu::active().native_word()
        {
            return Err(SchedulerPolicyError::Permission);
        }
        requested.nice = old.nice;
        Some(DeadlineParameters {
            runtime: requested.runtime,
            deadline: requested.deadline,
            period: requested.period,
        })
    } else {
        requested.nice = requested.nice.clamp(-20, 19);
        let limit = target
            .resource_limit(RLIMIT_NICE)
            .expect("RLIMIT_NICE must exist")
            .soft;
        if requested.nice < old.nice && !privileged && (20 - requested.nice) as u64 > limit {
            return Err(SchedulerPolicyError::Permission);
        }
        requested.runtime = 0;
        requested.deadline = 0;
        requested.period = 0;
        None
    };
    if old.flags & 1 != 0 && requested.flags & 1 == 0 && !privileged {
        return Err(SchedulerPolicyError::Permission);
    }
    if deadline != policy.deadline() {
        let carrier = target.carrier().ok_or(SchedulerPolicyError::NotFound)?;
        let times = deadline.map_or((0, 0, 0), |parameters| {
            (parameters.runtime, parameters.deadline, parameters.period)
        });
        carrier
            .handle()
            .set_deadline(crate::handles::resource(), times.0, times.1, times.2)
            .map_err(|status| match status {
                native_abi::ERR_NO_RESOURCES => SchedulerPolicyError::Busy,
                native_abi::ERR_ACCESS_DENIED => SchedulerPolicyError::Permission,
                native_abi::ERR_BAD_STATE => SchedulerPolicyError::NotFound,
                _ => SchedulerPolicyError::Invalid,
            })?;
    }
    policy.set_attributes(requested);
    let result = policy.attributes();
    drop(policy);
    request_task_reschedule(&target);
    Ok(result)
}
