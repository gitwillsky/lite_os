use crate::{
    fs::{InodeType, device::DeviceNumber, vfs},
    syscall::errno,
    task::current_task,
};

use super::pathname::{base, ferr, path};

const AT_REMOVEDIR: usize = 0x200;
const RENAME_NOREPLACE: u32 = 1;
const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFCHR: u32 = 0o020000;
const S_IFBLK: u32 = 0o060000;
const S_IFIFO: u32 = 0o010000;
const S_IFSOCK: u32 = 0o140000;

/// 按 Linux mknodat ABI 创建 regular file、FIFO、socket 或设备节点。
///
/// # Parameters
///
/// - `dirfd`: 相对 pathname 的目录 fd，或 AT_FDCWD。
/// - `name`: NUL 结尾且非空的 pathname。
/// - `mode`: inode type 与 permission/special bits；type 为零按 `S_IFREG` 处理。
/// - `device`: 设备节点的 `dev_t`（Linux 64-bit 编码）；其余类型忽略。
///
/// # Returns
///
/// 成功返回零。创建字符/块设备节点需要 `CAP_MKNOD`（以 effective UID 0 代表），否则 `EPERM`；
/// 目录类型返回 `EPERM`，未知 type 返回 `EINVAL`；pathname、重复、空间或只读错误返回对应 errno。
pub(crate) fn sys_mknodat(dirfd: isize, name: *const u8, mode: u32, device: u64) -> isize {
    let kind = match mode & S_IFMT {
        0 | S_IFREG => InodeType::File,
        S_IFIFO => InodeType::Fifo,
        S_IFSOCK => InodeType::Socket,
        S_IFCHR => InodeType::CharacterDevice,
        S_IFBLK => InodeType::BlockDevice,
        S_IFDIR => return -errno::EPERM,
        _ => return -errno::EINVAL,
    };
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let is_device = matches!(kind, InodeType::CharacterDevice | InodeType::BlockDevice);
    if is_device && task.credential_id(true, true) != 0 {
        return -errno::EPERM;
    }
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = match base(&task, dirfd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    vfs()
        .mknod_at(
            start,
            &path,
            kind,
            task.creation_mode(mode),
            is_device.then(|| DeviceNumber::decode(device)),
            &task.access_identity(true),
        )
        .map_or_else(ferr, |_| 0)
}

/// 按 Linux mkdirat ABI 创建目录。
///
/// # Parameters
///
/// - `dirfd`: 相对 pathname 的目录 fd，或 AT_FDCWD。
/// - `name`: NUL 结尾且非空的 pathname。
/// - `mode`: 新目录 permission bits；filesystem 应用类型位。
///
/// # Returns
///
/// 成功返回零；pathname、重复、空间、只读或 I/O 错误返回负 errno。
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

/// 按 Linux unlinkat ABI 删除普通目录项或空目录。
///
/// # Parameters
///
/// - `dirfd`: 相对 pathname 的目录 fd，或 AT_FDCWD。
/// - `name`: NUL 结尾且非空的 pathname。
/// - `flags`: 只接受 AT_REMOVEDIR。
///
/// # Returns
///
/// 成功返回零；flag、pathname、类型、非空目录或 I/O 错误返回负 errno。
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

/// 按 Linux renameat2 ABI 原子移动或替换单个 namespace entry。
///
/// # Parameters
///
/// - `old_dirfd`: old_name 为相对路径时的目录 fd。
/// - `old_name`: NUL 结尾的源 pathname。
/// - `new_dirfd`: new_name 为相对路径时的目录 fd。
/// - `new_name`: NUL 结尾的目标 pathname。
/// - `flags`: 零或 RENAME_NOREPLACE。
///
/// # Returns
///
/// 成功返回零；flag、跨 filesystem、类型、目录环或 I/O 错误返回负 errno。
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
