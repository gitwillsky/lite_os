#[cfg(not(test))]
use super::*;

#[cfg(not(test))]
pub(super) fn file_size_exceeded(task: &TaskControlBlock) -> isize {
    send_kernel_thread_signal(task.tgid(), task.tid(), 25).expect("current file writer must exist");
    -errno::EFBIG
}

/// @description 计算 non-append regular write 在 RLIMIT_FSIZE 内的可提交 prefix。
/// @param offset 当前文件 byte offset。
/// @param size_limit 当前 process 的 file-size soft limit。
/// @param requested 本批请求 byte 数。
/// @return 不越过 limit 的 byte 数；零表示当前 offset 已到边界。
pub(super) fn regular_write_allowance(offset: u64, size_limit: u64, requested: usize) -> usize {
    requested.min(usize::try_from(size_limit.saturating_sub(offset)).unwrap_or(usize::MAX))
}

/// @description 将下一批 staging 限制在 checked syscall request 的未完成 prefix 内。
/// @param total_length syscall vectors 的 checked 总长度。
/// @param completed 已由 backend 提交的 byte 数。
/// @param staging_capacity 已准备 staging slice 的固定容量。
/// @return 下一批最多可 stage 的 byte 数；request 完成时返回零。
pub(super) fn regular_write_chunk(
    total_length: usize,
    completed: usize,
    staging_capacity: usize,
) -> usize {
    total_length.saturating_sub(completed).min(staging_capacity)
}

/// @description 将普通文件的非 append write 截断到 RLIMIT_FSIZE 边界。
///
/// @param task 当前写入 task，提供 Process 级 file-size limit 与 SIGXFSZ target。
/// @param offset 本轮 write 的文件偏移。
/// @param requested 本轮期望复制的字节数。
/// @param completed 当前 syscall 已完成的字节数。
/// @return 可写字节数；边界上无进度时返回 SIGXFSZ/EFBIG，已有进度则返回 partial count。
///
/// caller 已证明目标是 non-append regular file；把 OFD 重新传入会令每个 page-sized chunk
/// 重复 clone inode 和读取 flags lock。
#[cfg(not(test))]
pub(super) fn bounded_regular_write(
    task: &TaskControlBlock,
    offset: u64,
    requested: usize,
    completed: usize,
) -> Result<usize, isize> {
    let allowed = regular_write_allowance(offset, task.file_size_limit(), requested);
    if allowed != 0 {
        return Ok(allowed);
    }
    Err(if completed == 0 {
        file_size_exceeded(task)
    } else {
        completed as isize
    })
}
