use crate::{
    fs::{FileSystemError, InodeType, vfs},
    syscall::errno,
    task::current_task,
};

use super::pathname::{base, ferr, path, path_allow_empty};

const AT_SYMLINK_FOLLOW: usize = 0x400;
const AT_EMPTY_PATH: usize = 0x1000;

/// @description 按 Linux symlinkat ABI 创建保存 raw target 的 symbolic link。
///
/// @param target NUL 结尾且不为空的 raw target pathname，不相对 new_dirfd 解析。
/// @param new_dirfd 新链接为相对路径时使用的目录 fd，或 AT_FDCWD。
/// @param new_path NUL 结尾的新链接 pathname。
/// @return 成功返回零；用户地址、pathname、空间、只读或 I/O 错误返回负 errno。
pub(crate) fn sys_symlinkat(target: *const u8, new_dirfd: isize, new_path: *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let target = match path(&task, target) {
        Ok(target) => target,
        Err(error) => return error,
    };
    let new_path = match path(&task, new_path) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let new_start = match base(&task, new_dirfd, &new_path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    vfs()
        .symlink_at(new_start, &new_path, &target)
        .map_or_else(ferr, |_| 0)
}

/// @description 按 Linux linkat ABI 为非目录 inode 创建同 filesystem 硬链接。
///
/// @param old_dirfd old_path 为相对路径时的目录 fd；AT_EMPTY_PATH 时为目标 fd。
/// @param old_path 默认不跟随 final symlink；AT_EMPTY_PATH 时允许空字符串。
/// @param new_dirfd 新链接为相对路径时使用的目录 fd，或 AT_FDCWD。
/// @param new_path NUL 结尾的新硬链接 pathname。
/// @param flags 只接受 AT_SYMLINK_FOLLOW 与 AT_EMPTY_PATH。
/// @return 成功返回零；flags、fd、类型、跨 filesystem 或底层 mutation 错误返回负 errno。
pub(crate) fn sys_linkat(
    old_dirfd: isize,
    old_path: *const u8,
    new_dirfd: isize,
    new_path: *const u8,
    flags: usize,
) -> isize {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let old_path = match path_allow_empty(&task, old_path) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let target = if old_path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return -errno::ENOENT;
        }
        match usize::try_from(old_dirfd)
            .ok()
            .and_then(|fd| task.fd_get(fd))
            .and_then(|ofd| ofd.inode_ref())
        {
            Some(inode) => inode,
            None => return -errno::EBADF,
        }
    } else {
        let old_start = match base(&task, old_dirfd, &old_path) {
            Ok(start) => start,
            Err(error) => return error,
        };
        let result = if flags & AT_SYMLINK_FOLLOW != 0 {
            vfs().open_at(old_start, &old_path)
        } else {
            vfs().open_at_no_follow(old_start, &old_path)
        };
        match result {
            Ok(inode) => inode,
            Err(error) => return ferr(error),
        }
    };
    if target.inode_type() == InodeType::Directory {
        return ferr(FileSystemError::PermissionDenied);
    }
    let new_path = match path(&task, new_path) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let new_start = match base(&task, new_dirfd, &new_path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    vfs()
        .link_at(target, new_start, &new_path)
        .map_or_else(ferr, |()| 0)
}
