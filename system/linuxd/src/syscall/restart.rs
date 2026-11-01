//! 标准 restart_syscall ABI；冻结的 semantic operation 由实际 Thread owner 提供。
use crate::task::{SyscallRestartBlock, current_task};

/// 消费 calling Thread 的一个 restart block；未登记操作返回 Linux EINTR。
pub(super) fn sys_restart_syscall() -> isize {
    let task = current_task().expect("restart_syscall requires a current task");
    match task.take_syscall_restart_block() {
        Some(SyscallRestartBlock::Futex {
            address,
            expected,
            private,
            deadline,
            bitset,
        }) => super::futex::wait_with_deadline(
            task,
            address,
            expected,
            private,
            Some(deadline),
            bitset,
        ),
        Some(SyscallRestartBlock::Sleep {
            deadline,
            remaining,
        }) => {
            drop(task);
            super::timer::sleep_with_deadline(deadline, remaining)
        }
        None => -super::errno::EINTR,
    }
}
