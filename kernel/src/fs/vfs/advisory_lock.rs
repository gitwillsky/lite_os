use alloc::{sync::Arc, vec::Vec};

use super::VirtualFileSystem;
use crate::fs::{FileSystemError, OpenFileDescription};

/// @description 一个 mounted inode 在本机 advisory-lock domain 内的稳定 identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct AdvisoryLockKey {
    filesystem: usize,
    inode: u64,
}

/// @description Linux flock 的两种持有模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdvisoryLockMode {
    Shared,
    Exclusive,
}

/// @description 一次非阻塞 lock-table mutation 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdvisoryLockAttempt {
    Acquired {
        key: AdvisoryLockKey,
        wake_waiters: bool,
    },
    Blocked {
        key: AdvisoryLockKey,
        wake_waiters: bool,
    },
}

/// @description prepared lock-table transaction 的无分配尝试结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PreparedLockAttempt {
    /// 尝试已线性化为取得或冲突。
    Complete(AdvisoryLockAttempt),
    /// 当前 table 形状超过锁外 staging capacity；未修改任何 lock state。
    NeedsStorage,
}

/// @description flock lock-record 分配或 backing inode 解析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdvisoryLockError {
    Unsupported,
    NoLocks,
    FileSystem(FileSystemError),
}

/// @description VFS advisory-lock owner 向 task wait owner 发布 inode 状态变化的反向 seam。
pub(crate) trait AdvisoryLockNotifier: Send + Sync {
    /// @description 唤醒指定 inode 上的全部 interruptible flock waiter 重新竞争。
    /// @param key 已释放或降级的 mounted inode identity。
    fn notify(&self, key: AdvisoryLockKey);
}

pub(super) struct AdvisoryFileLock {
    key: AdvisoryLockKey,
    exclusive: Option<usize>,
    shared: Vec<usize>,
}

/// @description BSD flock acquisition 的锁外 staging storage 与稳定 identity。
/// @ownership `table`/`shared` 只保存未发布或被替换的 Vec backing；成功 commit 后旧
/// backing 由本对象带到 lock 外析构。
pub(crate) struct PreparedAdvisoryLock {
    key: AdvisoryLockKey,
    owner: usize,
    requested: AdvisoryLockMode,
    table: Vec<AdvisoryFileLock>,
    shared: Vec<usize>,
}

impl VirtualFileSystem {
    pub(super) fn advisory_identity(
        ofd: &Arc<OpenFileDescription>,
    ) -> Result<(AdvisoryLockKey, usize), AdvisoryLockError> {
        let inode = ofd
            .opened_ref()
            .ok_or(AdvisoryLockError::Unsupported)?
            .inode();
        let metadata = inode.metadata().map_err(AdvisoryLockError::FileSystem)?;
        Ok((
            AdvisoryLockKey {
                filesystem: inode.filesystem_id(),
                inode: metadata.inode,
            },
            Arc::as_ptr(ofd) as usize,
        ))
    }

    /// @description 安装 advisory-lock 到 task wait registry 的唯一通知 adapter。
    /// @param notifier task layer 实现的无状态 wake adapter。
    pub(crate) fn set_advisory_lock_notifier(&self, notifier: Arc<dyn AdvisoryLockNotifier>) {
        let mut slot = self.advisory_lock_notifier.lock();
        assert!(slot.is_none(), "advisory-lock notifier installed twice");
        *slot = Some(notifier);
    }

    /// @description 解析 flock identity，但不分配或修改 lock table。
    /// @param ofd live open file description。
    /// @param requested shared/exclusive acquisition mode。
    /// @return 可跨 wait-registry 解锁窗口保留的 acquisition transaction。
    /// @errors anonymous OFD 或 inode metadata 失败。
    pub(crate) fn prepare_advisory_lock(
        &self,
        ofd: &Arc<OpenFileDescription>,
        requested: AdvisoryLockMode,
    ) -> Result<PreparedAdvisoryLock, AdvisoryLockError> {
        let (key, owner) = Self::advisory_identity(ofd)?;
        Ok(PreparedAdvisoryLock {
            key,
            owner,
            requested,
            table: Vec::new(),
            shared: Vec::new(),
        })
    }

    /// @description 按当前 flock table 形状在所有 owner lock 外扩充 staging storage。
    /// @param prepared 尚未提交的同 inode acquisition transaction。
    /// @return storage 足以覆盖观察到的 table；并发增长由最终尝试返回 `NeedsStorage`。
    /// @errors 容量算术或 backing allocation 失败返回 `NoLocks`。
    pub(crate) fn reserve_advisory_lock_storage(
        &self,
        prepared: &mut PreparedAdvisoryLock,
    ) -> Result<(), AdvisoryLockError> {
        let (table_required, shared_required) = {
            let locks = self.advisory_locks.lock();
            let existing = locks.iter().find(|entry| entry.key == prepared.key);
            let table_required = if existing.is_some() {
                0
            } else {
                locks
                    .len()
                    .checked_add(1)
                    .ok_or(AdvisoryLockError::NoLocks)?
            };
            let shared_required = if prepared.requested == AdvisoryLockMode::Shared {
                existing.map_or(1, |entry| entry.shared.len().saturating_add(1))
            } else {
                0
            };
            (table_required, shared_required)
        };
        prepared.table.clear();
        prepared.shared.clear();
        if prepared.table.capacity() < table_required {
            prepared
                .table
                .try_reserve_exact(table_required)
                .map_err(|_| AdvisoryLockError::NoLocks)?;
        }
        if prepared.shared.capacity() < shared_required {
            prepared
                .shared
                .try_reserve_exact(shared_required)
                .map_err(|_| AdvisoryLockError::NoLocks)?;
        }
        Ok(())
    }

    /// @description 在 flock owner 下复查冲突并以预留 Vec backing 无失败提交。
    /// @param prepared 锁外准备且 identity 不变的 acquisition transaction。
    /// @return acquired/blocked，或容量不足且 state 完全未修改的 `NeedsStorage`。
    pub(crate) fn try_prepared_advisory_lock(
        &self,
        prepared: &mut PreparedAdvisoryLock,
    ) -> PreparedLockAttempt {
        let mut locks = self.advisory_locks.lock();
        let Some(index) = locks.iter().position(|entry| entry.key == prepared.key) else {
            let Some(table_required) = locks.len().checked_add(1) else {
                return PreparedLockAttempt::NeedsStorage;
            };
            let shared_required = usize::from(prepared.requested == AdvisoryLockMode::Shared);
            if prepared.table.capacity() < table_required
                || prepared.shared.capacity() < shared_required
            {
                return PreparedLockAttempt::NeedsStorage;
            }
            prepared.table.clear();
            prepared.table.append(&mut locks);
            prepared.shared.clear();
            if prepared.requested == AdvisoryLockMode::Shared {
                prepared.shared.push(prepared.owner);
            }
            prepared.table.push(AdvisoryFileLock {
                key: prepared.key,
                exclusive: (prepared.requested == AdvisoryLockMode::Exclusive)
                    .then_some(prepared.owner),
                shared: core::mem::take(&mut prepared.shared),
            });
            core::mem::swap(&mut *locks, &mut prepared.table);
            return PreparedLockAttempt::Complete(AdvisoryLockAttempt::Acquired {
                key: prepared.key,
                wake_waiters: false,
            });
        };

        let state = &mut locks[index];
        let current = if state.exclusive == Some(prepared.owner) {
            Some(AdvisoryLockMode::Exclusive)
        } else if state.shared.contains(&prepared.owner) {
            Some(AdvisoryLockMode::Shared)
        } else {
            None
        };
        if current == Some(prepared.requested) {
            return PreparedLockAttempt::Complete(AdvisoryLockAttempt::Acquired {
                key: prepared.key,
                wake_waiters: false,
            });
        }
        if prepared.requested == AdvisoryLockMode::Shared
            && !state.shared.contains(&prepared.owner)
            && state
                .shared
                .len()
                .checked_add(1)
                .is_none_or(|required| prepared.shared.capacity() < required)
        {
            return PreparedLockAttempt::NeedsStorage;
        }

        // Capacity 已证明后才撤销旧模式；`NeedsStorage` 因而从不产生部分 conversion。
        let wake_waiters = current.is_some();
        if current == Some(AdvisoryLockMode::Exclusive) {
            state.exclusive = None;
        } else if current == Some(AdvisoryLockMode::Shared) {
            state
                .shared
                .retain(|candidate| *candidate != prepared.owner);
        }
        let compatible = match prepared.requested {
            AdvisoryLockMode::Shared => state.exclusive.is_none(),
            AdvisoryLockMode::Exclusive => state.exclusive.is_none() && state.shared.is_empty(),
        };
        if !compatible {
            return PreparedLockAttempt::Complete(AdvisoryLockAttempt::Blocked {
                key: prepared.key,
                wake_waiters,
            });
        }
        match prepared.requested {
            AdvisoryLockMode::Shared => {
                prepared.shared.clear();
                prepared.shared.extend_from_slice(&state.shared);
                prepared.shared.push(prepared.owner);
                core::mem::swap(&mut state.shared, &mut prepared.shared);
            }
            AdvisoryLockMode::Exclusive => state.exclusive = Some(prepared.owner),
        }
        PreparedLockAttempt::Complete(AdvisoryLockAttempt::Acquired {
            key: prepared.key,
            wake_waiters,
        })
    }

    /// @description 在 inode-wide lock table 内尝试取得或转换一个 OFD-owned flock。
    /// @param ofd live open file description；dup/fork descriptor 共享其 pointer identity。
    /// @param requested shared 或 exclusive 模式。
    /// @return 已取得或当前冲突；转换先释放旧模式，并标记是否需要唤醒其他 waiter。
    /// @errors anonymous OFD、lock record 内存不足或 inode metadata 失败。
    pub(crate) fn try_advisory_lock(
        &self,
        ofd: &Arc<OpenFileDescription>,
        requested: AdvisoryLockMode,
    ) -> Result<AdvisoryLockAttempt, AdvisoryLockError> {
        let mut prepared = self.prepare_advisory_lock(ofd, requested)?;
        loop {
            match self.try_prepared_advisory_lock(&mut prepared) {
                PreparedLockAttempt::Complete(attempt) => return Ok(attempt),
                PreparedLockAttempt::NeedsStorage => {
                    self.reserve_advisory_lock_storage(&mut prepared)?;
                }
            }
        }
    }

    fn remove_advisory_lock(
        &self,
        ofd: &Arc<OpenFileDescription>,
    ) -> Result<Option<AdvisoryLockKey>, AdvisoryLockError> {
        let (key, owner) = Self::advisory_identity(ofd)?;
        let removed = self.remove_advisory_lock_owner(owner);
        assert!(
            removed.is_none_or(|held_key| held_key == key),
            "one OFD cannot own advisory locks on different inodes"
        );
        Ok(removed)
    }

    /// @description 按 OFD identity 删除其唯一 flock record，不重新进入 backing filesystem。
    /// @param owner 最后一个 descriptor 正在关闭的 OFD pointer identity。
    /// @return 实际释放的 mounted inode identity；未持锁返回 None。
    fn remove_advisory_lock_owner(&self, owner: usize) -> Option<AdvisoryLockKey> {
        let mut locks = self.advisory_locks.lock();
        let index = locks
            .iter()
            .position(|entry| entry.exclusive == Some(owner) || entry.shared.contains(&owner))?;
        let state = &mut locks[index];
        let key = state.key;
        if state.exclusive == Some(owner) {
            state.exclusive = None;
        }
        state.shared.retain(|candidate| *candidate != owner);
        if state.exclusive.is_none() && state.shared.is_empty() {
            locks.swap_remove(index);
        }
        Some(key)
    }

    /// @description 显式释放一个 OFD 持有的 flock，并在状态变化后唤醒 waiter。
    /// @param ofd 任一 live duplicate descriptor 解析出的共享 OFD。
    /// @return 未持锁也按 Linux LOCK_UN 语义成功。
    /// @errors anonymous OFD 或 inode metadata 失败。
    pub(crate) fn unlock_advisory_lock(
        &self,
        ofd: &Arc<OpenFileDescription>,
    ) -> Result<(), AdvisoryLockError> {
        if let Some(key) = self.remove_advisory_lock(ofd)? {
            self.notify_advisory_lock(key);
        }
        Ok(())
    }

    /// @description 最后一个 duplicate descriptor 关闭时释放 OFD-owned flock。
    /// @param ofd descriptor_refs 已降为零、但 Arc 仍保持存活的 OFD。
    pub(crate) fn release_advisory_lock(&self, ofd: &Arc<OpenFileDescription>) {
        let owner = Arc::as_ptr(ofd) as usize;
        if let Some(key) = self.remove_advisory_lock_owner(owner) {
            self.notify_advisory_lock(key);
        }
    }

    /// @description 在不持 advisory lock-table 锁时投递一次状态变化通知。
    /// @param key waiter 需要重新竞争的 inode identity。
    pub(crate) fn notify_advisory_lock(&self, key: AdvisoryLockKey) {
        let notifier = self.advisory_lock_notifier.lock().clone();
        if let Some(notifier) = notifier {
            notifier.notify(key);
        }
    }
}
