use alloc::sync::Arc;

use super::TaskControlBlock;
use crate::fs::OpenFileDescription;

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
}
