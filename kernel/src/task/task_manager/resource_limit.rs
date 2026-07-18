use alloc::sync::Arc;

use super::{ProcessState, TASK_MANAGER};
use crate::memory::MemoryError;
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

/// @description 锁外预留 RLIMIT_NPROC 复检所需的 representative snapshot。
/// @ownership Vec 只持有尚未发布的 Arc snapshot；容量不足/OOM 时没有 graph mutation。
pub(super) struct ProcessSlotSnapshot {
    representatives: alloc::vec::Vec<(Arc<TaskControlBlock>, usize)>,
}

impl ProcessSlotSnapshot {
    /// @description 按当前 graph 大小在 IrqMutex 外预留 snapshot backing storage。
    /// @param minimum_capacity 并发增长重试要求的最小 representative 数。
    /// @return 空但容量充足的 snapshot transaction。
    /// @errors backing allocation 失败返回 OutOfMemory 且不持有 graph membership。
    pub(super) fn prepare(minimum_capacity: usize) -> Result<Self, MemoryError> {
        let observed = TASK_MANAGER
            .graph
            .lock()
            .nodes
            .values()
            .filter(
                |node| matches!(&node.state, ProcessState::Live(threads) if !threads.is_empty()),
            )
            .count();
        let mut representatives = alloc::vec::Vec::new();
        representatives
            .try_reserve_exact(observed.max(minimum_capacity))
            .map_err(|_| MemoryError::OutOfMemory)?;
        Ok(Self { representatives })
    }

    /// @description 在 process_creation guard 内捕获一次不分配的 live Process snapshot。
    /// @return 容量足够返回成功；并发增长超出容量时返回新的最小容量且不复制任何 Arc。
    pub(super) fn capture(&mut self) -> Result<(), usize> {
        debug_assert!(self.representatives.is_empty());
        let graph = TASK_MANAGER.graph.lock();
        let required = graph
            .nodes
            .values()
            .filter(
                |node| matches!(&node.state, ProcessState::Live(threads) if !threads.is_empty()),
            )
            .count();
        if required > self.representatives.capacity() {
            return Err(required);
        }
        self.representatives
            .extend(graph.nodes.values().filter_map(|node| {
                match &node.state {
                    ProcessState::Live(threads) => threads
                        .values()
                        .next()
                        .map(|task| (task.clone(), threads.len())),
                    ProcessState::Exited(_) => None,
                }
            }));
        Ok(())
    }

    /// @description 按 final snapshot 与 caller 当前 limit 判断是否保留一个创建 slot。
    /// @return root/unlimited 或同 real UID live thread 数低于 soft limit 时为 true。
    pub(super) fn allows_current(&self) -> bool {
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
        let count: usize = self
            .representatives
            .iter()
            .filter(|(task, _)| task.credential_id(true, false) == real_uid)
            .map(|(_, threads)| *threads)
            .sum();
        (count as u64) < limit
    }
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
