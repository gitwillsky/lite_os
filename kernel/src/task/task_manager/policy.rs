use super::thread_selector::scheduler_thread;
use super::{ProcessState, TASK_MANAGER};
use crate::task::{
    TaskControlBlock, current_task, model::RLIMIT_NICE, processor::request_task_reschedule,
};
use alloc::{sync::Arc, vec::Vec};

const SCHED_OTHER: i32 = 0;
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;

/// @description legacy Linux scheduler policy 操作。
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

/// @description legacy Linux scheduler policy operation 的领域错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerPolicyError {
    Access,
    Invalid,
    NotFound,
    Permission,
}

/// @description Linux get/setpriority 的 task collection selector。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchedulerNiceSelector {
    /// 零选择 caller，非零选择全局 TID。
    Process(u32),
    /// 零选择 caller process group，非零选择 PGID。
    Group(u32),
    /// 零选择 caller real UID，非零选择给定 UID。
    User(u32),
}

fn nice_targets(
    selector: SchedulerNiceSelector,
    caller: &Arc<TaskControlBlock>,
) -> Vec<Arc<TaskControlBlock>> {
    if let SchedulerNiceSelector::Process(tid) = selector {
        return scheduler_thread(tid as usize, caller).into_iter().collect();
    }
    let graph = TASK_MANAGER.graph.lock();
    let selected_group = match selector {
        SchedulerNiceSelector::Group(0) => {
            let Some(node) = graph.nodes.get(&caller.tgid()) else {
                return Vec::new();
            };
            Some(node.process_group)
        }
        SchedulerNiceSelector::Group(group) => Some(group as usize),
        SchedulerNiceSelector::Process(_) | SchedulerNiceSelector::User(_) => None,
    };
    let candidates = graph
        .nodes
        .values()
        .filter(|node| selected_group.is_none_or(|group| node.process_group == group))
        .filter_map(|node| match &node.state {
            ProcessState::Live(threads) => Some(threads.values().cloned()),
            ProcessState::Exited(_) => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    drop(graph);
    let SchedulerNiceSelector::User(requested_uid) = selector else {
        return candidates;
    };
    let uid = if requested_uid == 0 {
        caller.credential_id(true, false)
    } else {
        requested_uid
    };
    candidates
        .into_iter()
        .filter(|target| target.credential_id(true, false) == uid)
        .collect()
}

/// @description 查询或替换 Linux selector 命中的 live Thread nice 值。
///
/// @param selector PROCESS/PGRP/USER collection selector；零值由 calling Thread 解析。
/// @param replacement `None` 查询集合中最小 nice；`Some` 按 Linux 范围钳制后逐目标设置。
/// @return 查询返回最小 nice；设置成功返回规范化 nice。
/// @errors 空集合返回 `NotFound`；身份不匹配返回 `Permission`；提高优先级越过 RLIMIT_NICE 返回 `Access`。
pub(crate) fn scheduler_nice(
    selector: SchedulerNiceSelector,
    replacement: Option<i32>,
) -> Result<i32, SchedulerPolicyError> {
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    let targets = nice_targets(selector, &caller);
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

/// @description 查询或替换一个 live Thread 的 Linux I/O priority policy。
///
/// @param tid 零选择 calling Thread；正数使用全局 TID selector。
/// @param replacement None 查询，Some 替换已验证的 encoded priority。
/// @return 当前或新 I/O priority。
/// @errors caller/目标不存在返回 NotFound；设置其他身份目标且无 root 权限返回 Permission。
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

/// @description 查询或替换 live Thread 的 legacy Linux scheduler policy。
///
/// @param tid 零选择 calling Thread；正数使用 Linux 全局 TID selector。
/// @param request 查询、保留 policy 设置参数，或替换完整 legacy policy。
/// @return 当前 `SCHED_OTHER`，并在 owner 设置时包含 `SCHED_RESET_ON_FORK`。
/// @errors 目标不存在返回 `NotFound`；policy/priority 不可表达返回 `Invalid`；权限不足返回 `Permission`。
pub(crate) fn scheduler_policy(
    tid: usize,
    request: SchedulerPolicyRequest,
) -> Result<i32, SchedulerPolicyError> {
    // 1. 先解析 target，保持 Linux 对已复制参数的 ESRCH 优先级。
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    let target = scheduler_thread(tid, &caller).ok_or(SchedulerPolicyError::NotFound)?;
    if request == SchedulerPolicyRequest::Query {
        let reset = target.scheduling.policy.lock().reset_on_fork(None);
        return Ok(if reset {
            SCHED_RESET_ON_FORK
        } else {
            SCHED_OTHER
        });
    }

    // 2. 当前 scheduler 只可表达 SCHED_OTHER/priority 0；参数错误必须先于权限错误。
    let requested_reset = match request {
        SchedulerPolicyRequest::Query => unreachable!(),
        SchedulerPolicyRequest::SetParameters { priority } => {
            if priority != 0 {
                return Err(SchedulerPolicyError::Invalid);
            }
            None
        }
        SchedulerPolicyRequest::Replace { policy, priority } => {
            if priority != 0 || policy & !SCHED_RESET_ON_FORK != SCHED_OTHER {
                return Err(SchedulerPolicyError::Invalid);
            }
            Some(policy & SCHED_RESET_ON_FORK != 0)
        }
    };
    let privileged = caller
        .scheduler_privilege_for(&target)
        .ok_or(SchedulerPolicyError::Permission)?;

    // 3. policy lock 同时完成不可由普通 owner 清除的 reset flag 检查与替换。
    let mut policy = target.scheduling.policy.lock();
    let previous_reset = policy.reset_on_fork(None);
    if previous_reset && requested_reset == Some(false) && !privileged {
        return Err(SchedulerPolicyError::Permission);
    }
    if let Some(reset) = requested_reset {
        policy.reset_on_fork(Some(reset));
    }
    let reset = policy.reset_on_fork(None);
    Ok(if reset {
        SCHED_RESET_ON_FORK
    } else {
        SCHED_OTHER
    })
}

/// @description 查询 live Thread 可观察的 scheduler 基础时间片。
///
/// @param tid 零选择 calling Thread；正数使用 Linux 全局 TID selector。
/// @return timer owner 校准后的固定 `SCHED_OTHER` preemption quantum，单位纳秒。
/// @errors calling Thread 或目标不存在时返回 `NotFound`。
pub(crate) fn scheduler_rr_interval(tid: usize) -> Result<u64, SchedulerPolicyError> {
    let caller = current_task().ok_or(SchedulerPolicyError::NotFound)?;
    scheduler_thread(tid, &caller).ok_or(SchedulerPolicyError::NotFound)?;
    Ok(crate::timer::scheduler_quantum_ns())
}
