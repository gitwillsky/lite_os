//! kernel progress 工作复用唯一 timer deadline index 与无分配 TaskEvent wait。

use alloc::sync::{Arc, Weak};

use crate::{
    execution::sync::{IrqMutex, TaskEvent},
    fallible_tree::FallibleMap,
    memory::MemoryError,
};

use super::{DeadlineEntry, ExpiredTimer, TimerIdentity};

struct KernelTimerState {
    // OWNER: 唯一 node 在 idle state、active deadline index、expiry delivery 三者间转移。
    // expiry 必须先归还 node 再 signal；缺失该顺序会让被唤醒线程无法无分配地重装。
    entry: Option<DeadlineEntry>,
    expiration: Option<u64>,
}

pub(super) struct KernelTimer {
    id: u64,
    state: IrqMutex<KernelTimerState>,
    event: TaskEvent,
}

/// 一个后台执行体独占的可重复 deadline sleep；创建后 arm/wait/expiry 不再分配。
///
/// node 仍进入唯一 TimerQueue，阻塞仍由 TaskEvent 的标准 membership/arming 握手完成。
/// 不提供用户 ABI，不接受并发 waiter；`&mut self` 保证唯一调用方。
pub(crate) struct KernelDeadlineWait {
    timer: Arc<KernelTimer>,
}

impl KernelDeadlineWait {
    /// 在启动/正常容量阶段预留 timer object 与一个可反复重装的 deadline node。
    ///
    /// # Errors
    ///
    /// 任一 allocation 失败返回 OutOfMemory，不发布任何 active deadline。
    pub(crate) fn new() -> Result<Self, MemoryError> {
        let timer = Arc::try_new(KernelTimer {
            id: crate::id::next_runtime_object_id(),
            state: IrqMutex::new(KernelTimerState {
                entry: None,
                expiration: None,
            }),
            event: TaskEvent::new(),
        })
        .map_err(|_| MemoryError::OutOfMemory)?;
        let entry = FallibleMap::try_prepare(
            (0, TimerIdentity::Kernel(timer.id)),
            Some(Arc::downgrade(&timer)),
        )
        .map_err(|_| MemoryError::OutOfMemory)?;
        timer.state.lock().entry = Some(entry);
        Ok(Self { timer })
    }

    /// 阻塞当前 kernel task 到 absolute monotonic deadline，不进入可失败 wait preparation。
    ///
    /// # Parameters
    ///
    /// - `deadline`: monotonic nanosecond deadline；已到期立即返回。
    ///
    /// # Panics
    ///
    /// 当前没有 task context，或同一 owner 的上一次等待尚未完成时 fail-stop。
    pub(crate) fn sleep_until(&mut self, deadline: u64) {
        if deadline <= crate::execution::timer::get_time_ns() {
            return;
        }
        {
            let mut state = self.timer.state.lock();
            assert!(state.expiration.is_none(), "kernel deadline armed twice");
            let mut entry = state
                .entry
                .take()
                .expect("kernel deadline node not retired");
            entry.set_key((deadline, TimerIdentity::Kernel(self.timer.id)));
            state.expiration = Some(deadline);
            super::super::PROCESS_TABLE
                .timers
                .lock()
                .deadline_index
                .commit_vacant(entry);
        }
        // TaskEvent 的 pending/arming protocol 覆盖 expiry 在 waiter 登记前后的全部交错。
        // waiter target 只 clone current Arc；不创建 deadline registration、Vec 或树节点。
        // 后台 daemon 等待下一次工作，不是正在等待 IO；native TASK_IDLE 排除 load。
        self.timer.event.wait_idle();
    }
}

pub(in crate::execution::task::process_table) struct KernelTimerExpiration {
    timer: Arc<KernelTimer>,
    entry: DeadlineEntry,
}

impl KernelTimerExpiration {
    pub(super) fn from_entry(entry: DeadlineEntry) -> ExpiredTimer {
        match entry.value().as_ref().and_then(Weak::upgrade) {
            Some(timer) => ExpiredTimer::Kernel(Self { timer, entry }),
            None => {
                ExpiredTimer::Silent(super::TimerExpirationRetirement::from_deadline(Some(entry)))
            }
        }
    }

    /// timer queue 锁外归还同一 node，然后唤醒唯一 waiter。
    pub(in crate::execution::task::process_table) fn complete(self) {
        {
            let mut state = self.timer.state.lock();
            assert!(state.entry.is_none(), "kernel deadline node duplicated");
            assert!(
                state.expiration.take().is_some(),
                "kernel deadline not armed"
            );
            state.entry = Some(self.entry);
        }
        self.timer.event.signal();
    }
}

impl Drop for KernelTimer {
    fn drop(&mut self) {
        let expiration = self.state.lock().expiration;
        if let Some(expiration) = expiration {
            // 无 strong owner 后 Weak upgrade 不能再成功；expiry 已摘除的 orphan node
            // 会走 Silent。不存在 expiry delivery 与此 Drop 同时取得 live object。
            let retired = super::super::PROCESS_TABLE
                .timers
                .lock()
                .deadline_index
                .take_entry(&(expiration, TimerIdentity::Kernel(self.id)));
            drop(retired);
        }
    }
}
