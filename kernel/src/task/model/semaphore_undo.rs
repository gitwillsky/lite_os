//! Thread 的 SEM_UNDO membership；下层实际 adjustments 不属于 Task。
use super::TaskControlBlock;
use crate::ipc::{IpcError, SemaphoreUndoHandle, SemaphoreUndoList};
use alloc::sync::Arc;

impl TaskControlBlock {
    /// 非共享 clone 的分配在所有 Thread guard 外；共享只增加真实 participant，不复制 semadj。
    pub(super) fn prepare_semaphore_undo(
        &self,
        share: bool,
    ) -> Result<SemaphoreUndoHandle, IpcError> {
        if !share {
            return SemaphoreUndoHandle::new();
        }
        self.thread()
            .sem_undo
            .lock()
            .as_ref()
            .expect("live SEM_UNDO membership")
            .fork(true)
    }
    pub(crate) fn semaphore_undo(&self) -> Arc<SemaphoreUndoList> {
        self.thread()
            .sem_undo
            .lock()
            .as_ref()
            .expect("live SEM_UNDO membership")
            .list()
    }
    /// unshare/setns 先准备全部 fallible 资源，再替换并在旧 IPC namespace 退休前完成撤销。
    pub(super) fn replace_semaphore_undo(&self, next: SemaphoreUndoHandle) {
        let retired = self.thread().sem_undo.lock().replace(next);
        if let Some(mut retired) = retired {
            retired.retire(
                self.tgid_identity(),
                crate::timer::get_realtime_ns() / 1_000_000_000,
            );
        }
    }
    pub(in crate::task) fn retire_semaphore_undo(&self) {
        let retired = self.thread().sem_undo.lock().take();
        if let Some(mut retired) = retired {
            retired.retire(
                self.tgid_identity(),
                crate::timer::get_realtime_ns() / 1_000_000_000,
            );
        }
    }
}
