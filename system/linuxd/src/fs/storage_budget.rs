//! tmpfs/memfd 存储配额；上限、占用与 remount 提交只有一个 owner。

use alloc::sync::Arc;

use super::FileSystemError;
use crate::execution::sync::IrqMutex;

#[derive(Clone, Copy)]
struct Quota {
    limit: Option<u64>,
    used: u64,
}

impl Quota {
    fn reserve(&mut self) -> bool {
        let Some(next) = self.used.checked_add(1) else {
            return false;
        };
        if self.limit.is_some_and(|limit| next > limit) {
            return false;
        }
        self.used = next;
        true
    }

    fn release(&mut self) {
        self.used = self
            .used
            .checked_sub(1)
            .expect("storage quota released twice");
    }
}

struct BudgetState {
    pages: Quota,
    inodes: Quota,
}

/// 同锁只读投影；statfs 不能跨 remount 分别读取 limit/used。
pub(super) struct BudgetSnapshot {
    pub(super) page_limit: Option<u64>,
    pub(super) used_pages: u64,
    pub(super) inode_limit: Option<u64>,
    pub(super) used_inodes: u64,
}

/// 存储页与 inode 的生命周期配额；memfd 只使用页，tmpfs 共享完整 owner。
pub(super) struct StorageBudget {
    // OWNER: reserve/release/set_limits 同一 IRQ-safe 临界区，无 allocation/I/O/callback。
    // 两个独立 Atomic 的检查/CAS 不能排除 remount 已通过后按旧 limit 预留越额。
    state: IrqMutex<BudgetState>,
}

impl StorageBudget {
    /// 在 filesystem/file publication 前准备配额 owner；Arc allocation 可返回 OutOfMemory。
    pub(super) fn new(
        page_limit: Option<u64>,
        inode_limit: Option<u64>,
    ) -> Result<Arc<Self>, FileSystemError> {
        Arc::try_new(Self {
            state: IrqMutex::new(BudgetState {
                pages: Quota {
                    limit: page_limit,
                    used: 0,
                },
                inodes: Quota {
                    limit: inode_limit,
                    used: 0,
                },
            }),
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 返回同一次配额 transaction 的容量与占用，不复制可写状态。
    pub(super) fn snapshot(&self) -> BudgetSnapshot {
        let state = self.state.lock();
        BudgetSnapshot {
            page_limit: state.pages.limit,
            used_pages: state.pages.used,
            inode_limit: state.inodes.limit,
            used_inodes: state.inodes.used,
        }
    }

    /// 原子验证并提交两类上限；小于占用或追溯限制 unlimited 时返回 InvalidOperation，保持旧值。
    pub(super) fn set_limits(
        &self,
        pages: Option<u64>,
        inodes: Option<u64>,
    ) -> Result<(), FileSystemError> {
        let mut state = self.state.lock();
        // 固定 Linux shmem_reconfigure：unlimited 挂载不能在 remount 追溯加限。
        if pages.is_some() && state.pages.limit.is_none()
            || inodes.is_some() && state.inodes.limit.is_none()
            || pages.is_some_and(|limit| limit < state.pages.used)
            || inodes.is_some_and(|limit| limit < state.inodes.used)
        {
            return Err(FileSystemError::InvalidOperation);
        }
        state.pages.limit = pages;
        state.inodes.limit = inodes;
        Ok(())
    }

    /// 预留一页；成功后其存储页 owner 必须在最后一个引用释放时归还。
    pub(super) fn reserve_page(&self) -> bool {
        self.state.lock().pages.reserve()
    }
    /// 归还一个已预留页；重复释放 fail-stop。
    pub(super) fn release_page(&self) {
        self.state.lock().pages.release();
    }
    /// 预留一个 inode；成功后构造/失败回滚均归其 RAII owner。
    pub(super) fn reserve_inode(&self) -> bool {
        self.state.lock().inodes.reserve()
    }
    /// 归还一个已预留 inode；重复释放 fail-stop。
    pub(super) fn release_inode(&self) {
        self.state.lock().inodes.release();
    }
}
