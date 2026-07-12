use crate::{
    fs::{InodeType, vfs},
    syscall::errno,
    task::current_task,
};

use super::{base, ferr, path};

/// @description 按 Linux readlinkat ABI 读取末项 symbolic-link 的原始 target bytes。
///
/// @param fd 相对路径的目录 fd，或 AT_FDCWD；绝对路径忽略该值。
/// @param name NUL 结尾 pathname。
/// @param buffer 用户目标缓冲区；结果不追加 NUL。
/// @param size 最大复制字节数，零返回 EINVAL。
/// @return 实际复制长度；路径、类型、用户地址或 I/O 错误返回负 errno。
pub(crate) fn sys_readlinkat(fd: isize, name: *const u8, buffer: *mut u8, size: usize) -> isize {
    if size == 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = match base(&task, fd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let inode = match vfs().open_at_no_follow(start, &path) {
        Ok(inode) => inode,
        Err(error) => return ferr(error),
    };
    if inode.inode_type() != InodeType::SymLink {
        return -errno::EINVAL;
    }
    let target = match inode.read_link() {
        Ok(target) => target,
        Err(error) => return ferr(error),
    };
    let count = size.min(target.len());
    if task
        .copy_to_user(buffer as usize, &target[..count])
        .is_err()
    {
        return -errno::EFAULT;
    }
    count as isize
}
