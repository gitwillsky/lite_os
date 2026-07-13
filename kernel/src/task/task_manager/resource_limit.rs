use alloc::sync::Arc;

use super::{ProcessState, TASK_MANAGER};
use crate::task::{
    PendingSignal, RLIM_INFINITY, RLIMIT_NPROC, ResourceLimit, ResourceLimitError,
    TaskControlBlock, current_task,
};

fn representative(pid: usize) -> Option<Arc<TaskControlBlock>> {
    let graph = TASK_MANAGER.graph.lock();
    let ProcessState::Live(threads) = &graph.nodes.get(&pid)?.state else {
        return None;
    };
    threads.values().next().cloned()
}

/// @description 按 real UID 统计 live threads，实施 Linux RLIMIT_NPROC 创建门。
pub(in crate::task) fn check_process_slot() -> bool {
    let Some(caller) = current_task() else {
        return false;
    };
    let real_uid = caller.credential_id(true, false);
    if real_uid == 0 {
        return true;
    }
    let limit = caller.resource_limit(RLIMIT_NPROC).unwrap().soft;
    if limit == RLIM_INFINITY {
        return true;
    }
    let representatives: alloc::vec::Vec<_> = {
        let graph = TASK_MANAGER.graph.lock();
        let mut representatives = alloc::vec::Vec::new();
        if representatives.try_reserve(graph.nodes.len()).is_err() {
            return false;
        }
        representatives.extend(graph.nodes.values().filter_map(|node| {
            match &node.state {
                ProcessState::Live(threads) => threads
                    .values()
                    .next()
                    .map(|task| (task.clone(), threads.len())),
                ProcessState::Exited(_) => None,
            }
        }));
        representatives
    };
    let count: usize = representatives
        .into_iter()
        .filter(|(task, _)| task.credential_id(true, false) == real_uid)
        .map(|(_, threads)| threads)
        .sum();
    (count as u64) < limit
}

/// @description 在 runtime account 后按 Process 级 RLIMIT_CPU 投递 SIGXCPU/SIGKILL。
pub(super) fn enforce_cpu_limit(task: &Arc<TaskControlBlock>) {
    let runtime_us = task.process_cpu_runtime_us();
    let Some(signal) = task.resource_cpu_signal(runtime_us) else {
        return;
    };
    super::send_kernel_process_signal(task.tgid(), signal, PendingSignal::kernel());
}

/// @description 按 Linux prlimit64 permission 读取并可选替换一个 live Process 的 limit。
pub(crate) fn process_resource_limit(
    pid: usize,
    resource: usize,
    replacement: Option<ResourceLimit>,
) -> Result<ResourceLimit, ResourceLimitError> {
    let caller = current_task().ok_or(ResourceLimitError::NotFound)?;
    let target_pid = if pid == 0 { caller.tgid() } else { pid };
    let target = representative(target_pid).ok_or(ResourceLimitError::NotFound)?;
    let caller_uid = caller.credential_res_ids(true);
    let caller_gid = caller.credential_res_ids(false);
    let target_uid = target.credential_res_ids(true);
    let target_gid = target.credential_res_ids(false);
    let privileged = caller_uid[1] == 0;
    let same_identity = target_uid.iter().all(|target| *target == caller_uid[0])
        && target_gid.iter().all(|target| *target == caller_gid[0]);
    if caller.tgid() != target_pid && !privileged && !same_identity {
        return Err(ResourceLimitError::PermissionDenied);
    }
    match replacement {
        Some(replacement) => target.replace_resource_limit(resource, replacement, privileged),
        None => target
            .resource_limit(resource)
            .ok_or(ResourceLimitError::InvalidResource),
    }
}
