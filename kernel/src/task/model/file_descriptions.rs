use alloc::sync::Arc;

use super::TaskControlBlock;
use crate::fs::{OpenFileDescription, ProcFileDescriptorSnapshot};

impl TaskControlBlock {
    pub(crate) fn fd_get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.process.files.lock().get(fd)
    }

    /// @description 在 Process fd-table owner lock 内解析两个 descriptor 并执行一次操作。
    ///
    /// @param first 第一个 descriptor number。
    /// @param second 第二个 descriptor number。
    /// @param operation 只消费两个 live OFD identity、不得阻塞或再次获取本 Process fd-table。
    /// @return 任一 fd 不存在返回 `None`；否则返回 operation 结果。
    pub(crate) fn with_file_descriptions<R>(
        &self,
        first: usize,
        second: usize,
        operation: impl FnOnce(Arc<OpenFileDescription>, Arc<OpenFileDescription>) -> R,
    ) -> Option<R> {
        let files = self.process.files.lock();
        let first = files.get(first)?;
        let second = files.get(second)?;
        Some(operation(first, second))
    }

    /// @description 在 Process files lock 外解析 procfs fd targets，避免 VFS lock 进入 fd owner。
    /// @return 按 fd 递增的 target 快照；分配或 VFS 投影失败返回 None。
    pub(crate) fn process_file_descriptors(
        &self,
    ) -> Option<alloc::vec::Vec<ProcFileDescriptorSnapshot>> {
        let descriptions = self.process.files.lock().snapshot().ok()?;
        descriptions
            .into_iter()
            .map(|(fd, ofd)| {
                Some(ProcFileDescriptorSnapshot {
                    fd,
                    target: ofd.proc_target().ok()?,
                    opened: ofd.opened_ref(),
                })
            })
            .collect()
    }
}
