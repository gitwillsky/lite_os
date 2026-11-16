//! pivot_root 成功时，迁移仍精确引用原根的有效 Linux fs context。

use alloc::sync::Arc;

use super::{PROCESS_TABLE, ProcessState};
use crate::fs::OpenedFile;

/// 在 VFS 的 pivot 事务内更新 root/cwd，dirfd 保留实际挂载身份。
pub(crate) fn replace_fs_roots(old: &OpenedFile, new: &Arc<OpenedFile>) {
    let graph = PROCESS_TABLE.graph.lock();
    for (_, process) in graph.nodes.iter() {
        if let ProcessState::Live(threads) = &process.state {
            for (_, task) in threads.iter() {
                task.replace_fs_root(old, new);
            }
        }
    }
}
