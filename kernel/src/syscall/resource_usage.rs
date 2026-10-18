//! `getrusage`：按 Linux `struct rusage` 布局报告资源使用。

use crate::{syscall::errno, task::ResourceUsage, task::current_task};

const RUSAGE_SELF: i32 = 0;
const RUSAGE_CHILDREN: i32 = -1;
const RUSAGE_THREAD: i32 = 1;
/// `struct rusage`：两个 `timeval` 加 14 个 `long`。
pub(crate) const RUSAGE_BYTES: usize = 144;

/// 报告 calling Process、已回收 children 或 calling Thread 的资源使用。
///
/// user/system 时间来自 tick 采样的拆分，`ru_maxrss` 是在 exec、退出与读取时采样的 RSS 峰值，
/// 缺页、块 I/O 与上下文切换是精确计数；Linux 自己也不维护的字段（`ixrss/idrss/isrss/nswap/msgsnd/
/// msgrcv/nsignals`）恒为 0。
///
/// # Parameters
///
/// - `who`: `RUSAGE_SELF`、`RUSAGE_CHILDREN` 或 `RUSAGE_THREAD`。
/// - `usage`: 用户态 `struct rusage` 输出地址。
///
/// # Returns
///
/// 成功返回 0；`who` 非法返回 `-EINVAL`；输出地址不可写返回 `-EFAULT`。
pub(crate) fn sys_getrusage(who: i32, usage: usize) -> isize {
    let task = current_task().expect("getrusage requires a current task");
    let snapshot = match who {
        RUSAGE_SELF => task.process_resource_usage(),
        RUSAGE_THREAD => task.thread_resource_usage(),
        RUSAGE_CHILDREN => task.reaped_children_usage(),
        _ => return -errno::EINVAL,
    };
    if task.copy_to_user(usage, &encode_rusage(&snapshot)).is_err() {
        -errno::EFAULT
    } else {
        0
    }
}

/// 把使用量编码为 `struct rusage`。
pub(crate) fn encode_rusage(usage: &ResourceUsage) -> [u8; RUSAGE_BYTES] {
    // 每个字段 8 字节：utime{sec,usec}, stime{sec,usec}, maxrss, ixrss, idrss, isrss, minflt, majflt,
    // nswap, inblock, oublock, msgsnd, msgrcv, nsignals, nvcsw, nivcsw。
    let fields: [u64; RUSAGE_BYTES / size_of::<u64>()] = [
        usage.user_us / 1_000_000,
        usage.user_us % 1_000_000,
        usage.system_us / 1_000_000,
        usage.system_us % 1_000_000,
        usage.max_rss_kib,
        0,
        0,
        0,
        usage.minor_faults,
        usage.major_faults,
        0,
        usage.input_blocks,
        usage.output_blocks,
        0,
        0,
        0,
        usage.voluntary_switches,
        usage.involuntary_switches,
    ];
    let mut bytes = [0u8; RUSAGE_BYTES];
    for (chunk, field) in bytes.chunks_exact_mut(size_of::<u64>()).zip(fields) {
        chunk.copy_from_slice(&field.to_ne_bytes());
    }
    bytes
}
