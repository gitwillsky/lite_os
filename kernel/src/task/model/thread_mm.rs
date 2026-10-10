//! 每个 Thread 独立的 mm handle；其 root Arc 是实际映射 user，而非结构观察引用。
use alloc::sync::Arc;
use spin::Mutex;

use super::{AddressSpace, TaskControlBlock};
use crate::memory::MemoryError;
use crate::{memory::MemorySet, sync::TaskMutexWaitPreparation};

mod membership;
pub(super) use membership::MmMembership;

/// Process 只别名保活原 leader 的同一 outer handle，不另持 root Arc。
pub(super) type MmHandle = Arc<Mutex<MmMembership>>;

/// 构造尚未发布的独立 Thread handle；失败时释放输入的 root user。
pub(super) fn try_mm_handle(mm: Arc<AddressSpace>) -> Result<MmHandle, MemoryError> {
    Arc::try_new(Mutex::new(MmMembership::Live(mm))).map_err(|_| MemoryError::OutOfMemory)
}

impl TaskControlBlock {
    /// 不可失败的 mm cleanup acquisition；spin 锁外阻塞，取得 guard 后归还可复用 waiter。
    /// closure 不得再次取得同一 mm，实际页 backing OOM 仍按 user-access 语义处理。
    pub(super) fn with_cleanup_memory<R>(
        &self,
        mm: &AddressSpace,
        operation: impl FnOnce(&mut MemorySet) -> R,
    ) -> R {
        let mut wait = self.take_memory_cleanup_wait();
        let mut memory = mm.memory_set.lock_prepared(&mut wait);
        self.restore_memory_cleanup_wait(wait);
        operation(&mut memory)
    }

    pub(super) fn take_memory_cleanup_wait(&self) -> TaskMutexWaitPreparation {
        self.thread()
            .memory_cleanup_wait
            .lock()
            .take()
            .expect("nested mm cleanup waiter acquisition")
    }

    pub(super) fn restore_memory_cleanup_wait(&self, wait: TaskMutexWaitPreparation) {
        let previous = self.thread().memory_cleanup_wait.lock().replace(wait);
        assert!(
            previous.is_none(),
            "mm cleanup waiter replaced during acquisition"
        );
    }

    /// 取得 calling Thread 的 mmget user；任何此类 Arc 都保活 VMA/page，不得用于 mmgrab。
    pub(super) fn address_space(&self) -> Arc<AddressSpace> {
        self.mm_snapshot().expect("live Thread requires mm")
    }

    pub(super) fn mm_snapshot(&self) -> Option<Arc<AddressSpace>> {
        if self.is_kernel_thread() {
            None
        } else {
            self.thread().mm.lock().live().cloned()
        }
    }

    /// ptrace 在 credential snapshot 后取得同一 MM membership 的不可变视图。
    pub(super) fn memory_inspection(&self) -> MmMembership {
        if self.is_kernel_thread() {
            MmMembership::Retired {
                user_dumpable: false,
            }
        } else {
            self.thread().mm.lock().clone()
        }
    }
    /// namespace 的 single-threaded 检查先在 graph 锁外 pin 调用者的实际 mm。
    pub(in crate::task) fn namespace_mm(&self) -> Option<Arc<AddressSpace>> {
        if self.is_kernel_thread() {
            None
        } else {
            self.mm_snapshot()
        }
    }
    /// graph 锁内仅借用 mm handle 比较，不 clone/drop mm user，不进入 memory mutation。
    pub(in crate::task) fn shares_namespace_mm(&self, mm: &Arc<AddressSpace>) -> bool {
        !self.is_kernel_thread()
            && self
                .thread()
                .mm
                .lock()
                .live()
                .is_some_and(|own| Arc::ptr_eq(own, mm))
    }

    pub(in crate::task) fn has_mm(&self) -> bool {
        !self.is_kernel_thread() && self.thread().mm.lock().live().is_some()
    }

    /// PR_GET_DUMPABLE 读取 calling Thread 的共享 mm owner。
    pub(crate) fn user_dumpable(&self) -> bool {
        self.address_space().dumpable()
    }
    /// PR_SET_DUMPABLE 的参数已由 syscall codec 检查。
    pub(crate) fn set_user_dumpable(&self, user: bool) {
        self.address_space().set_dumpable(user);
    }

    /// 唯一的 exec replacement；旧 root 按值移交，不能另 clone 增加 mm_users。
    pub(super) fn replace_address_space(
        &self,
        replacement: Arc<AddressSpace>,
    ) -> Arc<AddressSpace> {
        self.thread().mm.lock().replace_live(replacement)
    }

    /// 移交 Thread 的最后 mm user；outer TCB/Process/wake 引用不延长其映射 lifetime。
    pub(super) fn take_address_space(&self) -> Arc<AddressSpace> {
        self.thread().mm.lock().retire()
    }
}

// Process 生命周期内唯一投影；graph/proc observer 不保活任何 TCB。
impl super::Process {
    pub(super) fn process_address_space(&self) -> Option<Arc<AddressSpace>> {
        let leader = self.leader_mm.lock().clone();
        leader.lock().live().cloned()
    }

    pub(super) fn process_memory_inspection(&self) -> MmMembership {
        let leader = self.leader_mm.lock().clone();
        leader.lock().clone()
    }
}
