use crate::{fs::InodeType, fs::vfs, syscall::errno, task::current_task};

use super::pathname::{base, ferr, path};

const AT_REMOVEDIR: usize = 0x200;
const RENAME_NOREPLACE: u32 = 1;
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;

/// @description 按 Linux mknodat ABI 创建普通文件 inode。
/// @param dirfd 相对 pathname 的目录 fd，或 AT_FDCWD。
/// @param name NUL 结尾且非空的 pathname。
/// @param mode inode type 与 permission/special bits；type 为零或 S_IFREG 时创建普通文件。
/// @param device character/block device 的编码；普通文件不使用该参数。
/// @return 成功返回零；不支持的 inode type、pathname、权限、空间或 I/O 错误返回负 errno。
pub(crate) fn sys_mknodat(dirfd: isize, name: *const u8, mode: u32, _device: u64) -> isize {
    if !matches!(mode & S_IFMT, 0 | S_IFREG) {
        return -errno::EOPNOTSUPP;
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
    vfs()
        .create_at(
            start,
            &path,
            InodeType::File,
            task.creation_mode(mode),
            &task.access_identity(true),
        )
        .map_or_else(ferr, |_| 0)
}

/// @description 按 Linux mkdirat ABI 创建目录。
/// @param dirfd 相对 pathname 的目录 fd，或 AT_FDCWD。
/// @param name NUL 结尾且非空的 pathname。
/// @param mode 新目录 permission bits；filesystem 应用类型位。
/// @return 成功返回零；pathname、重复、空间、只读或 I/O 错误返回负 errno。
pub(crate) fn sys_mkdirat(dirfd: isize, name: *const u8, mode: u32) -> isize {
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
    vfs()
        .create_at(
            start,
            &path,
            InodeType::Directory,
            task.creation_mode(mode),
            &task.access_identity(true),
        )
        .map_or_else(ferr, |_| 0)
}

/// @description 按 Linux unlinkat ABI 删除普通目录项或空目录。
/// @param dirfd 相对 pathname 的目录 fd，或 AT_FDCWD。
/// @param name NUL 结尾且非空的 pathname。
/// @param flags 只接受 AT_REMOVEDIR。
/// @return 成功返回零；flag、pathname、类型、非空目录或 I/O 错误返回负 errno。
pub(crate) fn sys_unlinkat(dirfd: isize, name: *const u8, flags: usize) -> isize {
    if flags & !AT_REMOVEDIR != 0 {
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
    vfs()
        .unlink_at(
            start,
            &path,
            flags & AT_REMOVEDIR != 0,
            &task.access_identity(true),
        )
        .map_or_else(ferr, |_| 0)
}

/// @description 按 Linux renameat2 ABI 原子移动或替换单个 namespace entry。
/// @param old_dirfd old_name 为相对路径时的目录 fd。
/// @param old_name NUL 结尾的源 pathname。
/// @param new_dirfd new_name 为相对路径时的目录 fd。
/// @param new_name NUL 结尾的目标 pathname。
/// @param flags 零或 RENAME_NOREPLACE。
/// @return 成功返回零；flag、跨 filesystem、类型、目录环或 I/O 错误返回负 errno。
pub(crate) fn sys_renameat2(
    old_dirfd: isize,
    old_name: *const u8,
    new_dirfd: isize,
    new_name: *const u8,
    flags: u32,
) -> isize {
    if flags & !RENAME_NOREPLACE != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let old_path = match path(&task, old_name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let new_path = match path(&task, new_name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let old_start = match base(&task, old_dirfd, &old_path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let new_start = match base(&task, new_dirfd, &new_path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    vfs()
        .rename_at(
            old_start,
            &old_path,
            new_start,
            &new_path,
            flags & RENAME_NOREPLACE != 0,
            &task.access_identity(true),
        )
        .map_or_else(ferr, |_| 0)
}
