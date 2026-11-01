use crate::{
    fs::{InodeType, vfs},
    syscall::errno,
    task::current_task,
};

use super::pathname::path_allow_empty;
use super::{base, ferr};

/// 按 Linux readlinkat ABI 读取末项 symbolic-link 的原始 target bytes。
///
/// # Parameters
///
/// - `fd`: 相对路径的目录 fd，或 AT_FDCWD；绝对路径忽略该值。
/// - `name`: NUL 结尾 pathname。
/// - `buffer`: 用户目标缓冲区；结果不追加 NUL。
/// - `size`: 最大复制字节数，零返回 EINVAL。
///
/// # Returns
///
/// 实际复制长度；路径、类型、用户地址或 I/O 错误返回负 errno。
pub(crate) fn sys_readlinkat(fd: isize, name: *const u8, buffer: *mut u8, size: usize) -> isize {
    if size == 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path_allow_empty(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let opened = if path.is_empty() {
        if fd == super::AT_FDCWD {
            task.lookup_origin().into_parts().1
        } else {
            match usize::try_from(fd).ok().and_then(|fd| task.fd_get_raw(fd)) {
                Some(file) => file.opened_ref(),
                None => return -errno::EBADF,
            }
        }
    } else {
        let start = match base(&task, fd, &path) {
            Ok(start) => start,
            Err(error) => return error,
        };
        match vfs().open_file_at_no_follow(start, &path, &task.access_identity(true)) {
            Ok(opened) => opened,
            Err(error) => return ferr(error),
        }
    };
    let inode = opened.inode();
    if inode.inode_type() != InodeType::SymLink {
        return -errno::EINVAL;
    }
    let target = match inode.read_link() {
        Ok(target) => target,
        Err(error) => return ferr(error),
    };
    opened.touch_access();
    let count = size.min(target.len());
    if task
        .copy_to_user(buffer as usize, &target[..count])
        .is_err()
    {
        return -errno::EFAULT;
    }
    count as isize
}
