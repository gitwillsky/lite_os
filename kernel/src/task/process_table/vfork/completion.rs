//! 原始 vfork child Thread 的唯一 completion token；graph/context cleanup 与 wake 分阶段。
use super::*;

pub(in crate::task::process_table) struct VforkCompletion {
    child: usize,
    parent: Arc<TaskControlBlock>,
}
impl VforkCompletion {
    /// 旧 user context/MM cleanup 完成后，锁外消费唯一 completion。
    pub(in crate::task::process_table) fn complete(self) {
        wake_parent(self.parent, self.child);
    }
}

/// Exit/exec 在 TID index 摘除或 promotion 前取走原始 child Thread 的 completion owner。
pub(in crate::task::process_table) fn take_vfork_completion(
    graph: &mut ProcessGraph,
    tid: usize,
) -> Option<VforkCompletion> {
    let index = graph.threads.get_mut(&tid)?;
    Some(VforkCompletion {
        child: index.tgid,
        parent: index.vfork_parent.take()?,
    })
}

/// 当前 image 旧 mm/context 已退休后，锁外完成原始 child Thread 的 vfork exec handoff。
pub(in crate::task) fn complete_vfork_exec(tid: usize) {
    let completion = take_vfork_completion(&mut PROCESS_TABLE.graph.lock(), tid);
    if let Some(completion) = completion {
        completion.complete();
    }
}
