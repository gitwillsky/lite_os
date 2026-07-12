use crate::{fs::vfs, syscall::errno, task::current_task};

use super::pathname::{base, ferr, path};

/// @description 按固定 root credential 基线实现 Linux faccessat pathname access 查询。
///
/// @param dirfd 相对 pathname 的目录 fd，或 AT_FDCWD。
/// @param name NUL 结尾且非空的 pathname。
/// @param mode F_OK，或 R_OK/W_OK/X_OK 的组合。
/// @return 存在且固定 root identity 允许访问时返回零；否则返回负 errno。
pub(crate) fn sys_faccessat(dirfd: isize, name: *const u8, mode: usize) -> isize {
    const X_OK: usize = 1;
    const ACCESS_MASK: usize = 7;
    if mode & !ACCESS_MASK != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = match base(&task, dirfd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let inode = match vfs().open_at(start, &path) {
        Ok(inode) => inode,
        Err(error) => return ferr(error),
    };
    if mode & 2 != 0 && inode.is_read_only() {
        return -errno::EROFS;
    }
    if mode & X_OK != 0 {
        let metadata = match inode.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return ferr(error),
        };
        if metadata.kind != crate::fs::InodeType::Directory && metadata.mode & 0o111 == 0 {
            return -errno::EACCES;
        }
    }
    0
}
