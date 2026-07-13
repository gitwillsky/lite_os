use alloc::{sync::Arc, vec::Vec};

use super::VirtualFileSystem;
use crate::fs::{AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, OpenFileDescription};

/// @description POSIX process-associated byte-range lock mode。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordLockMode {
    Read,
    Write,
}

/// @description 规范化后的半开 byte range；`end=None` 表示延伸到 EOF 之后。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordLockRange {
    pub(crate) start: u64,
    pub(crate) end: Option<u64>,
}

impl RecordLockRange {
    fn end_value(self) -> u128 {
        self.end.map(u128::from).unwrap_or(1u128 << 64)
    }

    fn overlaps(self, other: Self) -> bool {
        u128::from(self.start) < other.end_value() && u128::from(other.start) < self.end_value()
    }
}

/// @description `F_GETLK` 投影的第一个冲突 lock。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordLockConflict {
    pub(crate) owner: usize,
    pub(crate) mode: RecordLockMode,
    pub(crate) range: RecordLockRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RecordLock {
    key: AdvisoryLockKey,
    owner: usize,
    mode: RecordLockMode,
    range: RecordLockRange,
}

fn compatible(first: RecordLockMode, second: RecordLockMode) -> bool {
    first == RecordLockMode::Read && second == RecordLockMode::Read
}

fn maximum_end(first: Option<u64>, second: Option<u64>) -> Option<u64> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.max(second)),
        (None, _) | (_, None) => None,
    }
}

impl VirtualFileSystem {
    fn record_lock_key(
        ofd: &Arc<OpenFileDescription>,
    ) -> Result<AdvisoryLockKey, AdvisoryLockError> {
        Self::advisory_identity(ofd).map(|(key, _)| key)
    }

    /// @description 查询不同 Process 在 range 上持有的第一个不兼容 POSIX lock。
    ///
    /// @param ofd pathname-backed OFD。
    /// @param owner calling Process TGID。
    /// @param mode read/write requested mode。
    /// @param range 已按 whence 归一化的半开 byte range。
    /// @return 冲突 lock；不存在时返回 None。
    /// @errors anonymous OFD 或 inode metadata 失败。
    pub(crate) fn record_lock_conflict(
        &self,
        ofd: &Arc<OpenFileDescription>,
        owner: usize,
        mode: RecordLockMode,
        range: RecordLockRange,
    ) -> Result<Option<RecordLockConflict>, AdvisoryLockError> {
        let key = Self::record_lock_key(ofd)?;
        Ok(self
            .record_locks
            .lock()
            .iter()
            .filter(|lock| {
                lock.key == key
                    && lock.owner != owner
                    && lock.range.overlaps(range)
                    && !compatible(lock.mode, mode)
            })
            .min_by_key(|lock| lock.range.start)
            .map(|lock| RecordLockConflict {
                owner: lock.owner,
                mode: lock.mode,
                range: lock.range,
            }))
    }

    /// @description 原子取得、转换或释放一个 Process 的 POSIX byte-range lock。
    ///
    /// @param ofd pathname-backed OFD。
    /// @param owner calling Process TGID。
    /// @param requested Some(read/write) 取得或转换；None 解锁。
    /// @param range 已按 whence 归一化的半开 byte range。
    /// @return 已提交或被其他 Process 阻塞，并携带统一 inode wait key。
    /// @errors anonymous OFD、inode metadata 失败或 lock table 内存不足。
    pub(crate) fn try_record_lock(
        &self,
        ofd: &Arc<OpenFileDescription>,
        owner: usize,
        requested: Option<RecordLockMode>,
        range: RecordLockRange,
    ) -> Result<AdvisoryLockAttempt, AdvisoryLockError> {
        let key = Self::record_lock_key(ofd)?;
        let mut locks = self.record_locks.lock();
        if let Some(mode) = requested
            && locks.iter().any(|lock| {
                lock.key == key
                    && lock.owner != owner
                    && lock.range.overlaps(range)
                    && !compatible(lock.mode, mode)
            })
        {
            return Ok(AdvisoryLockAttempt::Blocked {
                key,
                wake_waiters: false,
            });
        }

        let extra = locks
            .iter()
            .filter(|lock| lock.key == key && lock.owner == owner && lock.range.overlaps(range))
            .count()
            .checked_mul(2)
            .and_then(|count| count.checked_add(usize::from(requested.is_some())))
            .ok_or(AdvisoryLockError::NoLocks)?;
        let mut next = Vec::new();
        next.try_reserve_exact(locks.len().saturating_add(extra))
            .map_err(|_| AdvisoryLockError::NoLocks)?;
        for lock in locks.iter().copied() {
            if lock.key != key || lock.owner != owner || !lock.range.overlaps(range) {
                next.push(lock);
                continue;
            }
            if lock.range.start < range.start {
                next.push(RecordLock {
                    range: RecordLockRange {
                        start: lock.range.start,
                        end: Some(range.start),
                    },
                    ..lock
                });
            }
            if let Some(end) = range.end
                && lock.range.end.is_none_or(|lock_end| end < lock_end)
            {
                next.push(RecordLock {
                    range: RecordLockRange {
                        start: end,
                        end: lock.range.end,
                    },
                    ..lock
                });
            }
        }
        if let Some(mode) = requested {
            next.push(RecordLock {
                key,
                owner,
                mode,
                range,
            });
        }
        next.sort_unstable_by_key(|lock| (lock.key, lock.owner, lock.range.start));
        let mut normalized: Vec<RecordLock> = Vec::new();
        normalized
            .try_reserve_exact(next.len())
            .map_err(|_| AdvisoryLockError::NoLocks)?;
        for lock in next {
            if let Some(previous) = normalized.last_mut()
                && previous.key == lock.key
                && previous.owner == lock.owner
                && previous.mode == lock.mode
                && previous.range.end_value() >= u128::from(lock.range.start)
            {
                previous.range.end = maximum_end(previous.range.end, lock.range.end);
            } else {
                normalized.push(lock);
            }
        }
        *locks = normalized;
        Ok(AdvisoryLockAttempt::Acquired {
            key,
            wake_waiters: true,
        })
    }

    /// @description 任一 descriptor close 时释放该 Process 在同一 inode 上的全部 POSIX locks。
    ///
    /// @param owner closing Process TGID。
    /// @param ofd 被关闭 descriptor 的 OFD。
    /// @return 无返回值；anonymous OFD 没有 record-lock state。
    pub(crate) fn release_record_locks_for_file(
        &self,
        owner: usize,
        ofd: &Arc<OpenFileDescription>,
    ) {
        let Ok(key) = Self::record_lock_key(ofd) else {
            return;
        };
        let changed = {
            let mut locks = self.record_locks.lock();
            let original = locks.len();
            locks.retain(|lock| lock.key != key || lock.owner != owner);
            locks.len() != original
        };
        if changed {
            self.notify_advisory_lock(key);
        }
    }

    /// @description Process exit 时释放其跨全部 inode 的 POSIX locks。
    ///
    /// @param owner exiting Process TGID。
    /// @return 无返回值；每个受影响 inode 的 waiter 都会被唤醒。
    pub(crate) fn release_process_record_locks(&self, owner: usize) {
        loop {
            let key = {
                let mut locks = self.record_locks.lock();
                let Some(key) = locks
                    .iter()
                    .find(|lock| lock.owner == owner)
                    .map(|lock| lock.key)
                else {
                    return;
                };
                locks.retain(|lock| lock.owner != owner || lock.key != key);
                key
            };
            self.notify_advisory_lock(key);
        }
    }
}
