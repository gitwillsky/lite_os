use alloc::sync::Arc;

use super::TaskControlBlock;
use crate::fs::{OpenedFile, Terminal};

impl TaskControlBlock {
    /// @description 复制当前 Process 工作目录的唯一 inode identity。
    /// @return 当前目录的共享 inode。
    pub(crate) fn working_directory(&self) -> Arc<OpenedFile> {
        self.process.cwd.lock().clone()
    }

    /// @description 原子替换当前 Process 的工作目录 identity。
    /// @param opened 已由 VFS 证明为目录的 opened entry。
    /// @return 无返回值。
    pub(crate) fn set_working_directory(&self, opened: Arc<OpenedFile>) {
        *self.process.cwd.lock() = opened;
    }

    /// @description 返回当前 Process 可继承的 platform Terminal identity。
    /// @return 与 console OFD 指向同一 TTY owner 的 Arc。
    pub(crate) fn terminal(&self) -> Arc<Terminal> {
        self.process.terminal.clone()
    }

    /// @description 返回当前 Process/thread group ID。
    /// @return TGID；Linux getpid 与 process-directed lookup 使用该值。
    pub(crate) fn tgid(&self) -> usize {
        self.process.tgid.0
    }

    /// @description 返回当前 Thread ID。
    /// @return 与 TGID 数值独立、由 ThreadContext 唯一拥有的 TID。
    pub(crate) fn tid(&self) -> usize {
        self.thread.tid
    }
}
