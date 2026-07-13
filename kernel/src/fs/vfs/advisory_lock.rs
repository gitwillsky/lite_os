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

impl VirtualFileSystem {
    fn advisory_identity(
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
        let (key, owner) = Self::advisory_identity(ofd)?;
        let mut locks = self.advisory_locks.lock();
        let index = locks.iter().position(|entry| entry.key == key);
        if index.is_none() {
            locks
                .try_reserve(1)
                .map_err(|_| AdvisoryLockError::NoLocks)?;
            let mut shared = Vec::new();
            if requested == AdvisoryLockMode::Shared {
                shared
                    .try_reserve(1)
                    .map_err(|_| AdvisoryLockError::NoLocks)?;
                shared.push(owner);
            }
            locks.push(AdvisoryFileLock {
                key,
                exclusive: (requested == AdvisoryLockMode::Exclusive).then_some(owner),
                shared,
            });
            return Ok(AdvisoryLockAttempt::Acquired {
                key,
                wake_waiters: false,
            });
        }

        let state = &mut locks[index.unwrap()];
        let current = if state.exclusive == Some(owner) {
            Some(AdvisoryLockMode::Exclusive)
        } else if state.shared.contains(&owner) {
            Some(AdvisoryLockMode::Shared)
        } else {
            None
        };
        if current == Some(requested) {
            return Ok(AdvisoryLockAttempt::Acquired {
                key,
                wake_waiters: false,
            });
        }
        if requested == AdvisoryLockMode::Shared && !state.shared.contains(&owner) {
            state
                .shared
                .try_reserve(1)
                .map_err(|_| AdvisoryLockError::NoLocks)?;
        }

        // 1. BSD flock conversion 先撤销旧模式；否则 SH→EX 会错误地原子升级。
        // 2. wake_waiters 记录该撤销，调用方必须在离开 wait-registry lock 后通知；缺失它会
        //    让与旧模式冲突的 waiter 永久睡眠。
        let wake_waiters = current.is_some();
        if current == Some(AdvisoryLockMode::Exclusive) {
            state.exclusive = None;
        } else if current == Some(AdvisoryLockMode::Shared) {
            state.shared.retain(|candidate| *candidate != owner);
        }

        let compatible = match requested {
            AdvisoryLockMode::Shared => state.exclusive.is_none(),
            AdvisoryLockMode::Exclusive => state.exclusive.is_none() && state.shared.is_empty(),
        };
        if !compatible {
            return Ok(AdvisoryLockAttempt::Blocked { key, wake_waiters });
        }
        match requested {
            AdvisoryLockMode::Shared => state.shared.push(owner),
            AdvisoryLockMode::Exclusive => state.exclusive = Some(owner),
        }
        Ok(AdvisoryLockAttempt::Acquired { key, wake_waiters })
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
