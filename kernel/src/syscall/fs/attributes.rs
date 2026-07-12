use crate::{
    fs::{FileSystemError, Inode, InodeType, vfs},
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
    let metadata = match inode.metadata() {
        Ok(value) => value,
        Err(error) => return ferr(error),
    };
    let identity = task.access_identity(true);
    if identity.uid() != 0 && identity.uid() != metadata.uid {
        return -errno::EPERM;
    }
    let mut mode = mode & 0o7777;
    if identity.uid() != 0 && !identity.in_group(metadata.gid) {
        mode &= !0o2000;
    }
    inode
        .set_owner_mode(Some(mode), None, None)
        .map_or_else(ferr, |()| 0)
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
    let metadata = match inode.metadata() {
        Ok(value) => value,
        Err(error) => return ferr(error),
    };
    let identity = task.access_identity(true);
    let uid = (owner != u32::MAX).then_some(owner);
    let gid = (group != u32::MAX).then_some(group);
    if identity.uid() != 0
        && (identity.uid() != metadata.uid
            || uid.is_some_and(|value| value != metadata.uid)
            || gid.is_some_and(|value| !identity.in_group(value)))
    {
        return -errno::EPERM;
    }
    let mode = (inode.inode_type() == InodeType::File && (uid.is_some() || gid.is_some()))
        .then_some(metadata.mode & !0o6000);
    inode.set_owner_mode(mode, uid, gid).map_or_else(
        |error| match error {
            FileSystemError::InvalidOperation => -errno::EOVERFLOW,
            other => ferr(other),
        },
        |()| 0,
    )
}
