use alloc::vec::Vec;

use super::wait_registry::IndexedWaitEntry;
use super::*;
use crate::fs::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, OpenFileDescription, RecordLockMode, RecordLockRange, vfs,
};

struct TaskAdvisoryLockNotifier;

impl AdvisoryLockNotifier for TaskAdvisoryLockNotifier {
    fn notify(&self, key: AdvisoryLockKey) {
        wake_advisory_lock_waiters(key);
    }
}

/// @description blocking flock acquisition 的 task-layer 失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdvisoryLockWaitError {
    Interrupted,
    Unsupported,
    NoLocks,
    FileSystem(crate::fs::FileSystemError),
}

impl From<AdvisoryLockError> for AdvisoryLockWaitError {
    fn from(error: AdvisoryLockError) -> Self {
        match error {
            AdvisoryLockError::Unsupported => Self::Unsupported,
            AdvisoryLockError::NoLocks => Self::NoLocks,
            AdvisoryLockError::FileSystem(error) => Self::FileSystem(error),
        }
    }
}

/// @description 在 task 初始化时安装 VFS advisory-lock 的唯一 wake adapter。
pub(crate) fn install_advisory_lock_notifier() {
    vfs().set_advisory_lock_notifier(Arc::new(TaskAdvisoryLockNotifier));
}

fn wake_advisory_lock_waiters(key: AdvisoryLockKey) -> usize {
    let mut waiters = Vec::new();
    let mut queue = INDEXED_WAIT_QUEUE.lock();
    while let Some((wait_id, entry)) = queue.take_advisory_lock(key) {
        waiters.push((wait_id, entry));
    }
    drop(queue);
    let count = waiters.len();
    for (wait_id, entry) in waiters {
        assert!(matches!(entry.kind, IndexedWaitKind::AdvisoryLock { .. }));
        crate::task::processor::wake_flock_task(entry.task, wait_id, WaitResult::Woken);
    }
    count
}

pub(super) fn interrupt_waiter(entry: IndexedWaitEntry, wait_id: u64, membership_id: u64) -> bool {
    assert_eq!(membership_id, wait_id);
    crate::task::processor::wake_flock_task(entry.task, wait_id, WaitResult::Interrupted)
}

/// @description 在统一 indexed wait owner 中执行任一种 inode advisory-lock 的阻塞竞争。
///
/// @param attempt 在 registry lock 内复查并尝试提交 lock 的无阻塞 closure。
/// @return 成功时 lock 已提交；signal、容量、backend 或 metadata 错误明确返回。
fn wait_for_file_lock(
    mut attempt: impl FnMut() -> Result<AdvisoryLockAttempt, AdvisoryLockError>,
) -> Result<(), AdvisoryLockWaitError> {
    let task = current_task().expect("file-lock wait requires current task");
    loop {
        // wait-registry → VFS lock-table 是唯一锁序。release 先放开 VFS table 再经 notifier
        // 获取 registry，因此 unlock 不可能落在 conflict recheck 与 membership publication 之间。
        let queue = INDEXED_WAIT_QUEUE.lock();
        let attempt = attempt()?;
        let (key, wake_waiters) = match attempt {
            AdvisoryLockAttempt::Acquired { key, wake_waiters } => {
                drop(queue);
                if wake_waiters {
                    vfs().notify_advisory_lock(key);
                }
                return Ok(());
            }
            AdvisoryLockAttempt::Blocked { key, wake_waiters } => (key, wake_waiters),
        };
        if task.has_deliverable_signal() {
            drop(queue);
            if wake_waiters {
                vfs().notify_advisory_lock(key);
            }
            return Err(AdvisoryLockWaitError::Interrupted);
        }
        let prepared =
            super::context_switch::prepare_current_block(&task, queue, |queue, current| {
                let wait_id = queue.insert_advisory_lock(key, current);
                WaitMembership::AdvisoryLock(wait_id)
            });
        if wake_waiters {
            vfs().notify_advisory_lock(key);
        }
        match prepared.suspend() {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(AdvisoryLockWaitError::Interrupted),
            WaitResult::TimedOut => panic!("file-lock wait cannot time out"),
        }
    }
}

/// @description 无丢失唤醒地阻塞到当前 OFD 取得 inode-wide BSD flock。
///
/// @param ofd caller descriptor 解析出的 live OFD；等待期间保持其生命周期。
/// @param mode shared 或 exclusive lock mode。
/// @return 成功时锁已经归该 OFD；signal、容量、backend 或 metadata 错误明确返回。
pub(crate) fn wait_for_advisory_lock(
    ofd: &Arc<OpenFileDescription>,
    mode: AdvisoryLockMode,
) -> Result<(), AdvisoryLockWaitError> {
    wait_for_file_lock(|| vfs().try_advisory_lock(ofd, mode))
}

/// @description 无丢失唤醒地阻塞到 calling Process 取得 POSIX byte-range lock。
///
/// @param ofd pathname-backed OFD。
/// @param owner calling Process TGID。
/// @param mode read/write lock mode。
/// @param range 已归一化的半开 byte range。
/// @return 成功时 lock 已提交；signal、容量、backend 或 metadata 错误明确返回。
pub(crate) fn wait_for_record_lock(
    ofd: &Arc<OpenFileDescription>,
    owner: usize,
    mode: RecordLockMode,
    range: RecordLockRange,
) -> Result<(), AdvisoryLockWaitError> {
    wait_for_file_lock(|| vfs().try_record_lock(ofd, owner, Some(mode), range))
}
