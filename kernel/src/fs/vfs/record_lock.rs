use alloc::{sync::Arc, vec::Vec};

use super::{VirtualFileSystem, advisory_lock::PreparedLockAttempt};
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

/// @description POSIX record-lock mutation 的锁外 staging storage 与稳定请求参数。
/// @ownership `next`/`normalized` 只保存未发布或替换下来的 table backing；最终 swap
/// 后旧 table 在 VFS owner lock 外析构。
pub(crate) struct PreparedRecordLock {
    key: AdvisoryLockKey,
    owner: usize,
    requested: Option<RecordLockMode>,
    range: RecordLockRange,
    next: Vec<RecordLock>,
    normalized: Vec<RecordLock>,
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

fn record_lock_storage_required(
    locks: &[RecordLock],
    key: AdvisoryLockKey,
    owner: usize,
    requested: Option<RecordLockMode>,
    range: RecordLockRange,
) -> Result<usize, AdvisoryLockError> {
    locks
        .iter()
        .filter(|lock| lock.key == key && lock.owner == owner && lock.range.overlaps(range))
        .count()
        .checked_mul(2)
        .and_then(|count| count.checked_add(usize::from(requested.is_some())))
        .and_then(|extra| locks.len().checked_add(extra))
        .ok_or(AdvisoryLockError::NoLocks)
}

impl VirtualFileSystem {
    fn record_lock_key(
        ofd: &Arc<OpenFileDescription>,
    ) -> Result<AdvisoryLockKey, AdvisoryLockError> {
        Self::advisory_identity(ofd).map(|(key, _)| key)
    }

    /// @description 解析 record-lock identity，但不分配或修改 lock table。
    /// @param ofd pathname-backed OFD。
    /// @param owner calling Process TGID。
    /// @param requested read/write acquisition 或 unlock。
    /// @param range 已规范化的半开 byte range。
    /// @return 可在 wait-registry 解锁窗口保留的 mutation transaction。
    /// @errors anonymous OFD 或 inode metadata 失败。
    pub(crate) fn prepare_record_lock(
        &self,
        ofd: &Arc<OpenFileDescription>,
        owner: usize,
        requested: Option<RecordLockMode>,
        range: RecordLockRange,
    ) -> Result<PreparedRecordLock, AdvisoryLockError> {
        Ok(PreparedRecordLock {
            key: Self::record_lock_key(ofd)?,
            owner,
            requested,
            range,
            next: Vec::new(),
            normalized: Vec::new(),
        })
    }

    /// @description 按当前 record-lock table 在所有 owner lock 外扩充两个 commit buffer。
    /// @param prepared 尚未提交的稳定 mutation transaction。
    /// @return storage 覆盖观察到的最坏 split 数；并发增长由最终尝试要求重试。
    /// @errors 容量算术或 backing allocation 失败返回 `NoLocks`。
    pub(crate) fn reserve_record_lock_storage(
        &self,
        prepared: &mut PreparedRecordLock,
    ) -> Result<(), AdvisoryLockError> {
        let required = {
            let locks = self.record_locks.lock();
            record_lock_storage_required(
                &locks,
                prepared.key,
                prepared.owner,
                prepared.requested,
                prepared.range,
            )?
        };
        prepared.next.clear();
        prepared.normalized.clear();
        if prepared.next.capacity() < required {
            prepared
                .next
                .try_reserve_exact(required)
                .map_err(|_| AdvisoryLockError::NoLocks)?;
        }
        if prepared.normalized.capacity() < required {
            prepared
                .normalized
                .try_reserve_exact(required)
                .map_err(|_| AdvisoryLockError::NoLocks)?;
        }
        Ok(())
    }

    /// @description 在 record-lock owner 下复查冲突并以预留双 buffer 无失败提交。
    /// @param prepared 锁外准备且 identity/range 不变的 mutation transaction。
    /// @return acquired/blocked，或 table 增长且 state 未修改的 `NeedsStorage`。
    pub(crate) fn try_prepared_record_lock(
        &self,
        prepared: &mut PreparedRecordLock,
    ) -> Result<PreparedLockAttempt, AdvisoryLockError> {
        let mut locks = self.record_locks.lock();
        if let Some(mode) = prepared.requested
            && locks.iter().any(|lock| {
                lock.key == prepared.key
                    && lock.owner != prepared.owner
                    && lock.range.overlaps(prepared.range)
                    && !compatible(lock.mode, mode)
            })
        {
            return Ok(PreparedLockAttempt::Complete(
                AdvisoryLockAttempt::Blocked {
                    key: prepared.key,
                    wake_waiters: false,
                },
            ));
        }
        let required = record_lock_storage_required(
            &locks,
            prepared.key,
            prepared.owner,
            prepared.requested,
            prepared.range,
        )?;
        if prepared.next.capacity() < required || prepared.normalized.capacity() < required {
            return Ok(PreparedLockAttempt::NeedsStorage);
        }

        prepared.next.clear();
        prepared.normalized.clear();
        for lock in locks.iter().copied() {
            if lock.key != prepared.key
                || lock.owner != prepared.owner
                || !lock.range.overlaps(prepared.range)
            {
                prepared.next.push(lock);
                continue;
            }
            if lock.range.start < prepared.range.start {
                prepared.next.push(RecordLock {
                    range: RecordLockRange {
                        start: lock.range.start,
                        end: Some(prepared.range.start),
                    },
                    ..lock
                });
            }
            if let Some(end) = prepared.range.end
                && lock.range.end.is_none_or(|lock_end| end < lock_end)
            {
                prepared.next.push(RecordLock {
                    range: RecordLockRange {
                        start: end,
                        end: lock.range.end,
                    },
                    ..lock
                });
            }
        }
        if let Some(mode) = prepared.requested {
            prepared.next.push(RecordLock {
                key: prepared.key,
                owner: prepared.owner,
                mode,
                range: prepared.range,
            });
        }
        prepared
            .next
            .sort_unstable_by_key(|lock| (lock.key, lock.owner, lock.range.start));
        for lock in prepared.next.drain(..) {
            if let Some(previous) = prepared.normalized.last_mut()
                && previous.key == lock.key
                && previous.owner == lock.owner
                && previous.mode == lock.mode
                && previous.range.end_value() >= u128::from(lock.range.start)
            {
                previous.range.end = maximum_end(previous.range.end, lock.range.end);
            } else {
                prepared.normalized.push(lock);
            }
        }
        core::mem::swap(&mut *locks, &mut prepared.normalized);
        Ok(PreparedLockAttempt::Complete(
            AdvisoryLockAttempt::Acquired {
                key: prepared.key,
                wake_waiters: true,
            },
        ))
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
        let mut prepared = self.prepare_record_lock(ofd, owner, requested, range)?;
        loop {
            match self.try_prepared_record_lock(&mut prepared)? {
                PreparedLockAttempt::Complete(attempt) => return Ok(attempt),
                PreparedLockAttempt::NeedsStorage => {
                    self.reserve_record_lock_storage(&mut prepared)?;
                }
            }
        }
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
