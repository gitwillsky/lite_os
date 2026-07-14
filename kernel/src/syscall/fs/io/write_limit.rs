use super::*;

pub(super) fn file_size_exceeded(task: &TaskControlBlock) -> isize {
    send_kernel_thread_signal(task.tgid(), task.tid(), 25).expect("current file writer must exist");
    -errno::EFBIG
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
pub(super) fn bounded_regular_write(
    task: &TaskControlBlock,
    offset: u64,
    requested: usize,
    completed: usize,
) -> Result<usize, isize> {
    let allowed = requested
        .min(usize::try_from(task.file_size_limit().saturating_sub(offset)).unwrap_or(usize::MAX));
    if allowed != 0 {
        return Ok(allowed);
    }
    Err(if completed == 0 {
        file_size_exceeded(task)
    } else {
        completed as isize
    })
}
