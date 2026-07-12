use crate::{
    fs::{FileSystemStatistics, vfs},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

use super::pathname::{ferr, path};

const STATFS_BYTES: usize = 120;

/// @description 按 Linux v7.1 RV64 `statfs` ABI 返回 pathname 所属 filesystem 的统计。
///
/// @param name NUL 结尾 raw pathname；相对路径从当前 cwd 解析。
/// @param address 用户态 120-byte `struct statfs` 输出地址。
/// @return 成功返回零；pathname、filesystem 或 copyout 失败返回负 errno。
pub(crate) fn sys_statfs(name: *const u8, address: usize) -> isize {
    let task = current_task().expect("statfs requires a current task");
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = (path.first() != Some(&b'/')).then(|| task.working_directory());
    let inode = match vfs().open_at(start, &path) {
        Ok(inode) => inode,
        Err(error) => return ferr(error),
    };
    let statistics = match vfs().statistics(inode) {
        Ok(statistics) => statistics,
        Err(error) => return ferr(error),
    };
    copy_statistics(&task, address, &statistics)
}

/// @description 按 Linux v7.1 RV64 `fstatfs` ABI 返回 descriptor backing filesystem 的统计。
///
/// @param fd 当前 Process 的 descriptor。
/// @param address 用户态 120-byte `struct statfs` 输出地址。
/// @return 成功返回零；无效 descriptor、filesystem 或 copyout 失败返回负 errno。
pub(crate) fn sys_fstatfs(fd: usize, address: usize) -> isize {
    let task = current_task().expect("fstatfs requires a current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let statistics = match ofd.filesystem_statistics() {
        Ok(statistics) => statistics,
        Err(error) => return ferr(error),
    };
    copy_statistics(&task, address, &statistics)
}

fn copy_statistics(
    task: &TaskControlBlock,
    address: usize,
    statistics: &FileSystemStatistics,
) -> isize {
    let mut bytes = [0u8; STATFS_BYTES];
    // 1. asm-generic RV64 使用 64-bit __kernel_long_t，并在 offset 56 放置两个 u32 fsid word。
    for (offset, value) in [
        statistics.magic,
        statistics.block_size,
        statistics.blocks,
        statistics.blocks_free,
        statistics.blocks_available,
        statistics.files,
        statistics.files_free,
    ]
    .into_iter()
    .enumerate()
    {
        write_u64(&mut bytes, offset * 8, value);
    }
    write_u32(&mut bytes, 56, statistics.fsid[0]);
    write_u32(&mut bytes, 60, statistics.fsid[1]);
    // 2. f_spare[4] 保持零；缺少清零会把 kernel stack 内容泄漏给 U-mode。
    write_u64(&mut bytes, 64, statistics.name_length);
    write_u64(&mut bytes, 72, statistics.fragment_size);
    write_u64(&mut bytes, 80, statistics.flags);
    if task.copy_to_user(address, &bytes).is_err() {
        -errno::EFAULT
    } else {
        0
    }
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}
