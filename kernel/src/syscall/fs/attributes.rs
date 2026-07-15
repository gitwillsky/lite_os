use crate::{
    fs::{FileSystemError, Inode, OwnerModeChange, vfs},
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

use super::pathname::{base, ferr, path_allow_empty};

const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const AT_EMPTY_PATH: u32 = 0x1000;

fn target(
    task: &TaskControlBlock,
    dirfd: isize,
    name: *const u8,
    flags: u32,
) -> Result<alloc::sync::Arc<dyn Inode>, isize> {
    let path = path_allow_empty(task, name)?;
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(-errno::ENOENT);
        }
        if task.access_identity(true).uid() != 0 {
            return Err(-errno::EPERM);
        }
        return usize::try_from(dirfd)
            .ok()
            .and_then(|fd| task.fd_get(fd))
            .and_then(|ofd| ofd.inode_ref())
            .ok_or(-errno::EBADF);
    }
    let start = base(task, dirfd, &path)?;
    let identity = task.access_identity(true);
    let result = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        vfs().open_at_no_follow(start, &path, &identity)
    } else {
        vfs().open_at(start, &path, &identity)
    };
    result.map_err(ferr)
}

fn chmod_inode(task: &TaskControlBlock, inode: alloc::sync::Arc<dyn Inode>, mode: u32) -> isize {
    inode
        .change_owner_mode(OwnerModeChange::chmod(task.access_identity(true), mode))
        .map_or_else(ferr, |()| 0)
}

/// @description 按 Linux fchmod ABI 修改已打开 inode 的 permission 与 special bits。
/// @param fd 指向 inode-backed open file description 的文件描述符。
/// @param mode 新的低 12-bit mode。
/// @return 成功为零，fd 无效返回 EBADF，其他失败返回对应负 errno。
pub(crate) fn sys_fchmod(fd: usize, mode: u32) -> isize {
    let task = current_task().expect("fchmod requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let inode = match ofd.inode_ref() {
        Some(inode) => inode,
        None => return -errno::EINVAL,
    };
    chmod_inode(&task, inode, mode)
}

/// @description 按 Linux fchmodat ABI 修改 inode permission 与 special bits。
/// @param dirfd 相对 pathname 的目录 fd。
/// @param name NUL 结尾 pathname。
/// @param mode 新的低 12-bit mode。
/// @return 成功为零，失败返回负 errno。
pub(crate) fn sys_fchmodat(dirfd: isize, name: *const u8, mode: u32) -> isize {
    let task = current_task().expect("fchmodat requires current task");
    let inode = match target(&task, dirfd, name, 0) {
        Ok(inode) => inode,
        Err(error) => return error,
    };
    chmod_inode(&task, inode, mode)
}

fn chown_inode(
    task: &TaskControlBlock,
    inode: alloc::sync::Arc<dyn Inode>,
    owner: u32,
    group: u32,
) -> isize {
    let uid = (owner != u32::MAX).then_some(owner);
    let gid = (group != u32::MAX).then_some(group);
    inode
        .change_owner_mode(OwnerModeChange::chown(task.access_identity(true), uid, gid))
        .map_or_else(
            |error| match error {
                FileSystemError::InvalidOperation => -errno::EOVERFLOW,
                other => ferr(other),
            },
            |()| 0,
        )
}

/// @description 按 Linux fchown ABI 修改已打开 inode 的 owner/group 并更新 ctime。
/// @param fd 指向 inode-backed open file description 的文件描述符。
/// @param owner u32::MAX 保留 UID，否则为新 owner。
/// @param group u32::MAX 保留 GID，否则为新 group。
/// @return 成功为零；fd 无效或 anonymous fd 返回 EBADF，其他失败返回负 errno。
pub(crate) fn sys_fchown(fd: usize, owner: u32, group: u32) -> isize {
    let task = current_task().expect("fchown requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(inode) = ofd.inode_ref() else {
        return -errno::EBADF;
    };
    chown_inode(&task, inode, owner, group)
}

/// @description 按 Linux fchownat ABI 原子修改 inode owner/group 并更新 ctime。
/// @param dirfd 相对 pathname 的目录 fd，或 AT_EMPTY_PATH 时的 fd。
/// @param name pathname；AT_EMPTY_PATH 时可为空。
/// @param owner u32::MAX 保留 UID，否则为新 owner。
/// @param group u32::MAX 保留 GID，否则为新 group。
/// @param flags 只接受 AT_SYMLINK_NOFOLLOW/AT_EMPTY_PATH。
/// @return 成功为零，失败返回负 errno。
pub(crate) fn sys_fchownat(
    dirfd: isize,
    name: *const u8,
    owner: u32,
    group: u32,
    flags: u32,
) -> isize {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("fchownat requires current task");
    let inode = match target(&task, dirfd, name, flags) {
        Ok(inode) => inode,
        Err(error) => return error,
    };
    chown_inode(&task, inode, owner, group)
}
