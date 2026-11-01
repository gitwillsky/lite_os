//! `membarrier(MEMBARRIER_CMD_PRIVATE_EXPEDITED)`：注册的地址空间请求时，让每个 CPU 执行一次完整
//! 内存屏障后返回（内核 `SYSTEM_BARRIER`）。运行该进程线程的 CPU 因此都越过了屏障。

use super::current_task;

/// 为当前线程的地址空间注册 private expedited 屏障。
pub(crate) fn register_private_memory_barrier() {
    current_task()
        .expect("membarrier syscall requires a current task")
        .register_private_memory_barrier();
}

/// 对已注册的地址空间执行屏障；未注册返回 false。
pub(crate) fn synchronize_private_memory() -> bool {
    let task = current_task().expect("membarrier syscall requires a current task");
    if !task.private_memory_barrier_registered() {
        return false;
    }
    native_runtime::system::barrier();
    true
}
