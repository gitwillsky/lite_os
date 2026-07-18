use super::errno;

/// @description 映射 fork/vfork 的 memory failure，不混淆 RLIMIT/PID exhaustion。
/// @param out_of_memory failure 是否来自 backing/page-table allocation。
/// @return 对应的负 Linux errno；OOM 为 ENOMEM，其他 memory failure 为 EAGAIN。
pub(super) const fn process_clone_memory_errno(out_of_memory: bool) -> isize {
    if out_of_memory {
        -errno::ENOMEM
    } else {
        -errno::EAGAIN
    }
}

/// @description 映射 pthread-shaped clone 的 memory failure。
/// @param out_of_memory failure 是否来自 backing/page-table allocation。
/// @return 对应的负 Linux errno；OOM 为 ENOMEM，其他 memory failure 为 EINVAL。
pub(super) const fn thread_clone_memory_errno(out_of_memory: bool) -> isize {
    if out_of_memory {
        -errno::ENOMEM
    } else {
        -errno::EINVAL
    }
}

/// @description 映射 RLIMIT_NPROC 或 PID namespace exhaustion。
/// @return Linux EAGAIN。
pub(super) const fn clone_resource_errno() -> isize {
    -errno::EAGAIN
}
