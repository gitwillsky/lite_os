use crate::arch::hart;

use super::current_task;

/// @description 为当前 Task 的 AddressSpace 注册 private expedited memory barrier。
///
/// @return 无返回值；当前 syscall 必须存在 current Task。
pub(crate) fn register_private_memory_barrier() {
    current_task()
        .expect("membarrier syscall requires a current task")
        .register_private_memory_barrier();
}

/// @description 对已注册 AddressSpace 执行同步 private memory barrier。
///
/// @return 已注册并完成所有 active hart 屏障时返回 true；未注册返回 false。
pub(crate) fn synchronize_private_memory() -> bool {
    let task = current_task().expect("membarrier syscall requires a current task");
    if !task.private_memory_barrier_registered() {
        return false;
    }
    hart::synchronize_memory_barrier();
    true
}
