use alloc::vec::Vec;

use super::wait_registry::IndexedWaitEntry;
use super::*;
use crate::fs::{
    AdvisoryLockAttempt, AdvisoryLockError, AdvisoryLockKey, AdvisoryLockMode,
    AdvisoryLockNotifier, OpenFileDescription, vfs,
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

/// @description 无丢失唤醒地阻塞到当前 OFD 取得 inode-wide advisory flock。
/// @param ofd caller descriptor 解析出的 live OFD；等待期间保持其生命周期。
/// @param mode shared 或 exclusive lock mode。
/// @return 成功时锁已经归该 OFD；signal、容量、backend 或 metadata 错误明确返回。
pub(crate) fn wait_for_advisory_lock(
    ofd: &Arc<OpenFileDescription>,
    mode: AdvisoryLockMode,
) -> Result<(), AdvisoryLockWaitError> {
    let task = current_task().expect("flock wait requires current task");
    let cpu = hart_id();
    loop {
        // wait-registry → VFS lock-table 是唯一锁序。release 先放开 VFS table 再经 notifier
        // 获取 registry，因此 unlock 不可能落在 conflict recheck 与 membership publication 之间。
        let mut queue = INDEXED_WAIT_QUEUE.lock();
        let attempt = vfs().try_advisory_lock(ofd, mode)?;
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
        let end_time = get_time_us();
        let mut sched = task.scheduling.policy.lock();
        let runtime = end_time.saturating_sub(sched.last_runtime);
        sched.update_vruntime(runtime);
        drop(sched);
        with_current_processor(|processor| {
            let current = processor
                .take_current()
                .expect("flock wait requires current task");
            assert!(Arc::ptr_eq(&current, &task));
            let mut scheduling = task.scheduling.state.lock();
            assert_eq!(scheduling.run_state, RunState::Running { cpu });
            assert!(scheduling.wait.is_none());
            assert!(scheduling.wait_result.is_none());
            let wait_id = queue.insert_advisory_lock(key, current);
            scheduling.wait = Some(WaitMembership::AdvisoryLock(wait_id));
            scheduling.run_state = RunState::Blocking { cpu };
        });
        drop(queue);
        if wake_waiters {
            vfs().notify_advisory_lock(key);
        }
        schedule_with_task_context(task.clone());
        match task
            .scheduling
            .state
            .lock()
            .wait_result
            .take()
            .expect("flock waiter resumed without result")
        {
            WaitResult::Woken => {}
            WaitResult::Interrupted => return Err(AdvisoryLockWaitError::Interrupted),
            WaitResult::TimedOut => panic!("flock wait cannot time out"),
        }
    }
}
