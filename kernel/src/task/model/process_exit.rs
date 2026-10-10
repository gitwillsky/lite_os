//! Thread 注销后的旧地址空间清理 token；exec 替换 Process handle 不改变此 token 的 backing。

use alloc::sync::Arc;

use super::{AddressSpace, TaskControlBlock};
use crate::memory::UserFaultLimits;
use crate::sync::TaskMutexWaitPreparation;

/// 退出线程对旧 mm 的最后一次 clear-child-tid publication。
///
/// OWNER: token 独占已取走的 clear-child-tid，并保活旧 AddressSpace；缺失 pin 会在 graph
/// 注销后与 sibling exec 竞争，向新映像写零或派生错误 futex key。它不复制 MemorySet。
#[must_use = "graph 注销后必须完成旧地址空间的 clear-child-tid publication"]
pub(in crate::task) struct UserExitMemory {
    address_space: Arc<AddressSpace>,
    fault_limits: UserFaultLimits,
    clear_child_tid: Option<usize>,
    wait: TaskMutexWaitPreparation,
}

impl TaskControlBlock {
    /// 在 graph 仍计数本 Thread 时完成 robust 和 context retirement，并冻结旧 mm completion。
    ///
    /// # Returns
    ///
    /// 无分配的清理 token；调用方必须在 graph 注销之后消费它，才能发布 pthread join completion。
    pub(in crate::task) fn prepare_user_exit_memory(&self) -> UserExitMemory {
        let fault_limits = self.user_fault_limits();
        self.cleanup_robust_list();
        // 先退休裸 UserContext pointer 和 temporary trap VMA，再允许 thread_count 变成一。
        // 此时 graph membership 阻止既有 single-thread exec 提交，所有 blocking cleanup 仍有 wait target。
        self.remove_thread_trap_context();
        self.retire_fs_context();
        self.retire_file_table();
        self.retire_semaphore_undo();
        self.retire_shared_memory_creator();
        self.retire_namespaces();
        self.retire_seccomp();
        UserExitMemory {
            address_space: self.take_address_space(),
            fault_limits,
            clear_child_tid: self.take_clear_child_tid(),
            wait: self.take_memory_cleanup_wait(),
        }
    }
}

/// exec 退休旧映像的唯一 mm user/clearTid registration，复用同一 release 语义。
pub(super) fn release_exec_mm(
    task: &TaskControlBlock,
    address_space: Arc<AddressSpace>,
    clear_child_tid: Option<usize>,
    fault_limits: UserFaultLimits,
) {
    let wait = UserExitMemory {
        address_space,
        clear_child_tid,
        fault_limits,
        wait: task.take_memory_cleanup_wait(),
    }
    .complete();
    task.restore_memory_cleanup_wait(wait);
}

impl UserExitMemory {
    /// graph 注销后，只在被保活的旧 mm 清 TID 并唤醒同一 backing 上的一个 futex waiter。
    pub(in crate::task) fn complete(mut self) -> TaskMutexWaitPreparation {
        let Some(address) = self.clear_child_tid else {
            return self.wait;
        };
        // 此 Arc 是从 Thread handle 移出的唯一 user；Weak registries/outer aliases 不计数。
        // 其他 Thread、proc 或 mapping callback user 才能令 mm_users > 1。
        if Arc::strong_count(&self.address_space) == 1 {
            return self.wait;
        }
        let _ = crate::task::process_table::futex_wake_with_key(
            1,
            u32::MAX,
            &mut self.wait,
            |wait, consume| {
                let mut memory = self.address_space.memory_set.lock_prepared(wait);
                // Linux mm_release 忽略 put_user 错误后仍尝试 WAKE；只读地址可合法派生 key。
                let _ = memory.copy_to_user(address, &0u32.to_ne_bytes(), self.fault_limits);
                let identity = core::ptr::from_ref(self.address_space.as_ref()).addr();
                let key = memory.futex_key(address, identity, false, self.fault_limits)?;
                // consume 只保存 scalar key；此 closure 归还 mm guard 后才执行 registry/scheduler wake。
                consume(key);
                Ok(())
            },
        );
        self.wait
    }
}
