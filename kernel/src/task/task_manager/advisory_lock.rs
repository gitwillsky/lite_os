use super::*;
use crate::fs::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, OpenFileDescription, PreparedAdvisoryLock, PreparedLockAttempt,
    PreparedRecordLock, RecordLockMode, RecordLockRange, vfs,
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
    vfs().set_advisory_lock_notifier(
        Arc::try_new(TaskAdvisoryLockNotifier).expect("advisory-lock notifier allocation failed"),
    );
}

fn wake_advisory_lock_waiters(key: AdvisoryLockKey) -> usize {
    let mut count = 0;
    while let Some(wake) = WAIT_REGISTRY.wake_advisory_one(key) {
        count += 1;
        if let Some(claimed) = wake.claimed {
            assert!(matches!(claimed.kind, IndexedWaitKind::AdvisoryLock));
            crate::task::processor::wake_flock_task(claimed.task, claimed.id, WaitResult::Woken);
        }
    }
    count
}

/// @description 在统一 indexed wait owner 中执行任一种 inode advisory-lock 的阻塞竞争。
///
/// @param transaction 持有稳定 inode identity 与全部锁外 staging storage。
/// @param reserve_storage 仅在没有 registry/VFS owner guard 时扩充 transaction。
/// @param attempt 在 registry shard lock 外复查并无分配提交的 closure。
/// @return 成功时 lock 已提交；signal、容量、backend 或 metadata 错误明确返回。
fn wait_for_file_lock<Prepared>(
    mut transaction: Prepared,
    mut reserve_storage: impl FnMut(&mut Prepared) -> Result<(), AdvisoryLockError>,
    mut attempt: impl FnMut(&mut Prepared) -> Result<PreparedLockAttempt, AdvisoryLockError>,
) -> Result<(), AdvisoryLockWaitError> {
    let task = current_task().expect("file-lock wait requires current task");
    loop {
        let ticket = WAIT_REGISTRY.allocate_ticket();
        let first_attempt = loop {
            match attempt(&mut transaction)? {
                PreparedLockAttempt::Complete(attempt) => break attempt,
                PreparedLockAttempt::NeedsStorage => {
                    reserve_storage(&mut transaction)?;
                }
            }
        };
        let (key, wake_waiters) = match first_attempt {
            AdvisoryLockAttempt::Acquired { key, wake_waiters } => {
                if wake_waiters {
                    vfs().notify_advisory_lock(key);
                }
                return Ok(());
            }
            AdvisoryLockAttempt::Blocked { key, wake_waiters } => (key, wake_waiters),
        };
        if wake_waiters {
            vfs().notify_advisory_lock(key);
        }
        let wait = match ticket.prepare_advisory_lock(key, task.clone()) {
            Ok(wait) => wait,
            Err(()) => {
                let attempt = loop {
                    match attempt(&mut transaction)? {
                        PreparedLockAttempt::Complete(attempt) => break attempt,
                        PreparedLockAttempt::NeedsStorage => {
                            reserve_storage(&mut transaction)?;
                        }
                    }
                };
                let interrupted = task.has_deliverable_signal();
                match attempt {
                    AdvisoryLockAttempt::Acquired { key, wake_waiters } => {
                        if wake_waiters {
                            vfs().notify_advisory_lock(key);
                        }
                        return Ok(());
                    }
                    AdvisoryLockAttempt::Blocked { key, wake_waiters } => {
                        if wake_waiters {
                            vfs().notify_advisory_lock(key);
                        }
                        return Err(if interrupted {
                            AdvisoryLockWaitError::Interrupted
                        } else {
                            AdvisoryLockWaitError::NoLocks
                        });
                    }
                }
            }
        };

        // 先发布 Arming node，再在全部 registry shard lock 外复查 VFS conflict；unlock
        // notifier 若并发命中只把 registration 标记为 Notified，不会唤醒尚未 Blocking 的 task。
        let published = WAIT_REGISTRY.publish(wait);
        let attempt = loop {
            let result = match attempt(&mut transaction) {
                Ok(result) => result,
                Err(error) => {
                    let _ = published.cancel();
                    return Err(error.into());
                }
            };
            match result {
                PreparedLockAttempt::Complete(attempt) => break attempt,
                PreparedLockAttempt::NeedsStorage => {
                    if let Err(error) = reserve_storage(&mut transaction) {
                        let _ = published.cancel();
                        return Err(error.into());
                    }
                }
            }
        };
        let (confirmed_key, wake_waiters) = match attempt {
            AdvisoryLockAttempt::Acquired { key, wake_waiters } => {
                let _ = published.cancel();
                if wake_waiters {
                    vfs().notify_advisory_lock(key);
                }
                return Ok(());
            }
            AdvisoryLockAttempt::Blocked { key, wake_waiters } => (key, wake_waiters),
        };
        if confirmed_key != key {
            let _ = published.cancel();
            if wake_waiters {
                vfs().notify_advisory_lock(confirmed_key);
            }
            continue;
        }
        if task.has_deliverable_signal() {
            let result = published.cancel();
            if wake_waiters {
                vfs().notify_advisory_lock(confirmed_key);
            }
            match result {
                CancelOutcome::Cancelled | CancelOutcome::Notified(WaitResult::Interrupted) => {
                    return Err(AdvisoryLockWaitError::Interrupted);
                }
                CancelOutcome::Notified(WaitResult::Woken) => continue,
                CancelOutcome::Notified(WaitResult::TimedOut | WaitResult::OutOfMemory) => {
                    unreachable!("advisory wait has no deadline/OOM notification")
                }
            }
        }
        let arm = match published.prepare_arm() {
            Ok(arm) => arm,
            Err(WaitResult::Woken) => continue,
            Err(WaitResult::Interrupted) => return Err(AdvisoryLockWaitError::Interrupted),
            Err(WaitResult::TimedOut | WaitResult::OutOfMemory) => unreachable!(),
        };
        let prepared = super::context_switch::prepare_current_block(&task, arm, |arm, _| {
            WaitMembership::AdvisoryLock(arm.arm())
        });
        if wake_waiters {
            vfs().notify_advisory_lock(confirmed_key);
        }
        match prepared.suspend() {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(AdvisoryLockWaitError::Interrupted),
            WaitResult::TimedOut => panic!("file-lock wait cannot time out"),
            WaitResult::OutOfMemory => unreachable!("wait OOM is returned before blocking"),
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
    let transaction: PreparedAdvisoryLock = vfs().prepare_advisory_lock(ofd, mode)?;
    wait_for_file_lock(
        transaction,
        |transaction| vfs().reserve_advisory_lock_storage(transaction),
        |transaction| Ok(vfs().try_prepared_advisory_lock(transaction)),
    )
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
    let transaction: PreparedRecordLock =
        vfs().prepare_record_lock(ofd, owner, Some(mode), range)?;
    wait_for_file_lock(
        transaction,
        |transaction| vfs().reserve_record_lock_storage(transaction),
        |transaction| vfs().try_prepared_record_lock(transaction),
    )
}
