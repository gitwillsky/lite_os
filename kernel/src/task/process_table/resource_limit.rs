use alloc::sync::Arc;

use super::{PROCESS_TABLE, ProcessState};
use crate::memory::MemoryError;
use crate::task::{
    PendingSignal, RLIM_INFINITY, RLIMIT_NPROC, ResourceLimit, ResourceLimitError,
    TaskControlBlock, current_task,
};

/// 锁外预留 RLIMIT_NPROC 复检所需的逐 Thread snapshot。
///
/// Vec 只持有尚未发布的 Arc snapshot；容量不足/OOM 时没有 graph mutation。
pub(super) struct ThreadSlotSnapshot {
    threads: alloc::vec::Vec<Arc<TaskControlBlock>>,
}

impl ThreadSlotSnapshot {
    /// 按当前 graph 大小在 IrqMutex 外预留 snapshot backing storage。
    ///
    /// # Parameters
    ///
    /// - `minimum_capacity`: 并发增长重试要求的最小 Thread 数。
    ///
    /// # Returns
    ///
    /// 空但容量充足的 snapshot transaction。
    ///
    /// # Errors
    ///
    /// backing allocation 失败返回 OutOfMemory 且不持有 graph membership。
    pub(super) fn prepare(minimum_capacity: usize) -> Result<Self, MemoryError> {
        let observed = PROCESS_TABLE
            .graph
            .lock()
            .nodes
            .values()
            .map(|node| match &node.state {
                ProcessState::Live(threads) => threads.len(),
                ProcessState::Exited(..) => 0,
            })
            .sum::<usize>();
        let mut threads = alloc::vec::Vec::new();
        threads
            .try_reserve_exact(observed.max(minimum_capacity))
            .map_err(|_| MemoryError::OutOfMemory)?;
        Ok(Self { threads })
    }

    /// 在 process_creation guard 内捕获一次不分配的 live Process snapshot。
    ///
    /// # Returns
    ///
    /// 容量足够返回成功；并发增长超出容量时返回新的最小容量且不复制任何 Arc。
    pub(super) fn capture(&mut self) -> Result<(), usize> {
        debug_assert!(self.threads.is_empty());
        let graph = PROCESS_TABLE.graph.lock();
        let required = graph
            .nodes
            .values()
            .map(|node| match &node.state {
                ProcessState::Live(threads) => threads.len(),
                ProcessState::Exited(..) => 0,
            })
            .sum::<usize>();
        if required > self.threads.capacity() {
            return Err(required);
        }
        for node in graph.nodes.values() {
            if let ProcessState::Live(threads) = &node.state {
                self.threads.extend(threads.values().cloned());
            }
        }
        Ok(())
    }

    /// 按 final snapshot 与 caller 当前 limit 判断是否保留一个创建 slot。
    ///
    /// # Returns
    ///
    /// root/unlimited 或同 real UID live thread 数低于 soft limit 时为 true。
    pub(super) fn allows_current(&self) -> bool {
        let Some(caller) = current_task() else {
            return false;
        };
        let real_uid = caller.credential_id(true, false);
        if real_uid == 0
            || caller.has_initial_capability(syscall_abi::capability::Capability::SYS_RESOURCE)
            || caller.has_initial_capability(syscall_abi::capability::Capability::SYS_ADMIN)
        {
            return true;
        }
        let limit = caller.resource_limit(RLIMIT_NPROC).unwrap().soft;
        if limit == RLIM_INFINITY {
            return true;
        }
        let count = self
            .threads
            .iter()
            .filter(|task| task.credential_id(true, false) == real_uid)
            .count();
        (count as u64) < limit
    }
}

/// graph 已锁定的 calling Thread credential commit capability。
///
/// 只借 canonical ProcessNode，清除 PDEATH 时同时更新其计数；不得递归取得 graph。
pub(in crate::task) struct CredentialTransition<'a> {
    node: &'a mut super::ProcessNode,
    task: &'a TaskControlBlock,
}
impl CredentialTransition<'_> {
    pub(in crate::task) fn clear_parent_death(&mut self) {
        if self.task.parent_death_signal(Some(0)) != 0 {
            self.node.pdeath_enabled_threads -= 1;
        }
    }
}

/// identity transition 与 clone admission 共用 creation→graph→credentials 锁序。
///
/// callback 仅执行无分配、不阻塞的 candidate scalar commit；MM 必须提前 pin，
/// dumpability 与 PDEATH consequence 先于身份 publication，最终 Arc/组值锁外退休。
pub(in crate::task) fn with_credential_transition<T>(
    task: &TaskControlBlock,
    commit: impl FnOnce(&mut CredentialTransition<'_>) -> T,
) -> T {
    let _creation = PROCESS_TABLE.process_creation.lock();
    let mut graph = PROCESS_TABLE.graph.lock();
    let node = graph
        .nodes
        .get_mut(&task.tgid())
        .expect("credential commit requires the calling live Process");
    commit(&mut CredentialTransition { node, task })
}

/// 在 runtime account 后按 Process 级 RLIMIT_CPU 投递 SIGXCPU/SIGKILL。
pub(super) fn enforce_cpu_limit(task: &Arc<TaskControlBlock>) {
    if task.is_kernel_thread() {
        return;
    }
    let runtime_us = task.process_cpu_runtime_us();
    let Some(signal) = task.resource_cpu_signal(runtime_us) else {
        return;
    };
    super::send_kernel_process_signal(task.tgid(), signal, PendingSignal::kernel());
}

/// 按 Linux prlimit64 permission 读取并可选替换一个 live/zombie Process 的 limit。
pub(crate) fn process_resource_limit(
    pid: usize,
    resource: usize,
    replacement: Option<ResourceLimit>,
) -> Result<ResourceLimit, ResourceLimitError> {
    let caller = current_task().ok_or(ResourceLimitError::NotFound)?;
    // prlimit 的非零 pid 是 TID selector，权限只豁免指定的 calling Thread。
    // 原 leader 已退出时，Process 仍保留其 credential owner 与共享 limits。
    let selected = if pid == 0 {
        caller.tid()
    } else {
        caller
            .resolve_pid(pid)
            .ok_or(ResourceLimitError::NotFound)?
    };
    let (process, target_identity) = if pid == 0 {
        (caller.process().clone(), caller.credential_snapshot())
    } else {
        let graph = PROCESS_TABLE.graph.lock();
        if let Some((_, thread)) = super::thread_selector::thread_by_tid(&graph, selected) {
            (thread.process().clone(), thread.credential_snapshot())
        } else {
            let node = graph
                .nodes
                .get(&selected)
                .ok_or(ResourceLimitError::NotFound)?;
            (
                node.process.clone(),
                node.process.process_credential_snapshot(),
            )
        }
    };
    let caller_identity = caller.credential_snapshot();
    let privileged =
        caller.has_initial_capability(syscall_abi::capability::Capability::SYS_RESOURCE);
    let same_identity = target_identity
        .uids
        .iter()
        .all(|uid| *uid == caller_identity.uids[0])
        && target_identity
            .gids
            .iter()
            .all(|gid| *gid == caller_identity.gids[0]);
    let target_authority = caller.has_namespace_capability(
        &target_identity.user_namespace,
        syscall_abi::capability::Capability::SYS_RESOURCE,
    );
    if pid != 0 && caller.tid() != selected && !target_authority && !same_identity {
        return Err(ResourceLimitError::PermissionDenied);
    }
    match replacement {
        Some(replacement) => process.replace_resource_limit(resource, replacement, privileged),
        None => process
            .resource_limit(resource)
            .ok_or(ResourceLimitError::InvalidResource),
    }
}
