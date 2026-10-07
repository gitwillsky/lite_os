use crate::{
    fs::{
        FileSystemError, IN_ATTRIB, Inode, OpenedFile, OwnerModeChange, notify_opened, notify_self,
        vfs,
    },
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
) -> Result<Target, isize> {
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
            .and_then(|ofd| Some(Target::new(ofd.inode_ref()?, ofd.opened_ref())))
            .ok_or(-errno::EBADF);
    }
    let start = base(task, dirfd, &path)?;
    let identity = task.access_identity(true);
    let result = if flags & AT_SYMLINK_NOFOLLOW != 0 {
        vfs().open_file_at_no_follow(start, &path, &identity)
    } else {
        vfs().open_file_at(start, &path, &identity)
    };
    result
        .map(|opened| Target::new(opened.inode(), Some(opened)))
        .map_err(ferr)
}

/// 属性变更的目标：inode，以及（有路径时）用于向父目录 watch 投递 `IN_ATTRIB` 的 opened entry。
struct Target {
    inode: alloc::sync::Arc<dyn Inode>,
    opened: Option<alloc::sync::Arc<OpenedFile>>,
}

impl Target {
    fn new(
        inode: alloc::sync::Arc<dyn Inode>,
        opened: Option<alloc::sync::Arc<OpenedFile>>,
    ) -> Self {
        Self { inode, opened }
    }

    /// 变更成功之后通知 inotify：有 opened entry 时同时通知父目录的 watch。
    fn changed(&self) {
        match &self.opened {
            Some(opened) => notify_opened(opened, IN_ATTRIB),
            None => notify_self(self.inode.as_ref(), IN_ATTRIB),
        }
    }
}

fn chmod_inode(task: &TaskControlBlock, target: Target, mode: u32) -> isize {
    if let Err(error) = vfs().require_writable(target.inode.filesystem_id()) {
        return ferr(error);
    }
    let result = target
        .inode
        .change_owner_mode(OwnerModeChange::chmod(task.access_identity(true), mode));
    if result.is_ok() {
        target.changed();
    }
    result.map_or_else(ferr, |()| 0)
}

/// 按 Linux fchmod ABI 修改已打开 inode 的 permission 与 special bits。
///
/// # Parameters
///
/// - `fd`: 指向 inode-backed open file description 的文件描述符。
/// - `mode`: 新的低 12-bit mode。
///
/// # Returns
///
/// 成功为零，fd 无效返回 EBADF，其他失败返回对应负 errno。
pub(crate) fn sys_fchmod(fd: usize, mode: u32) -> isize {
    let task = current_task().expect("fchmod requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(inode) = ofd.inode_ref() else {
        return -errno::EINVAL;
    };
    chmod_inode(&task, Target::new(inode, ofd.opened_ref()), mode)
}

/// 按 Linux fchmodat ABI 修改 inode permission 与 special bits。
///
/// # Parameters
///
/// - `dirfd`: 相对 pathname 的目录 fd。
/// - `name`: NUL 结尾 pathname。
/// - `mode`: 新的低 12-bit mode。
///
/// # Returns
///
/// 成功为零，失败返回负 errno。
pub(crate) fn sys_fchmodat(dirfd: isize, name: *const u8, mode: u32) -> isize {
    let task = current_task().expect("fchmodat requires current task");
    let target = match target(&task, dirfd, name, 0) {
        Ok(target) => target,
        Err(error) => return error,
    };
    chmod_inode(&task, target, mode)
}

fn chown_inode(task: &TaskControlBlock, target: Target, owner: u32, group: u32) -> isize {
    let uid = (owner != u32::MAX).then_some(owner);
    let gid = (group != u32::MAX).then_some(group);
    if let Err(error) = vfs().require_writable(target.inode.filesystem_id()) {
        return ferr(error);
    }
    let result = target.inode.change_owner_mode(OwnerModeChange::chown(
        task.access_identity(true),
        uid,
        gid,
    ));
    if result.is_ok() {
        target.changed();
    }
    result.map_or_else(
        |error| match error {
            FileSystemError::InvalidOperation => -errno::EOVERFLOW,
            other => ferr(other),
        },
        |()| 0,
    )
}

/// 按 Linux fchown ABI 修改已打开 inode 的 owner/group 并更新 ctime。
///
/// # Parameters
///
/// - `fd`: 指向 inode-backed open file description 的文件描述符。
/// - `owner`: u32::MAX 保留 UID，否则为新 owner。
/// - `group`: u32::MAX 保留 GID，否则为新 group。
///
/// # Returns
///
/// 成功为零；fd 无效或 anonymous fd 返回 EBADF，其他失败返回负 errno。
pub(crate) fn sys_fchown(fd: usize, owner: u32, group: u32) -> isize {
    let task = current_task().expect("fchown requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(inode) = ofd.inode_ref() else {
        return -errno::EBADF;
    };
    chown_inode(&task, Target::new(inode, ofd.opened_ref()), owner, group)
}

/// 按 Linux fchownat ABI 原子修改 inode owner/group 并更新 ctime。
///
/// # Parameters
///
/// - `dirfd`: 相对 pathname 的目录 fd，或 AT_EMPTY_PATH 时的 fd。
/// - `name`: pathname；AT_EMPTY_PATH 时可为空。
/// - `owner`: u32::MAX 保留 UID，否则为新 owner。
/// - `group`: u32::MAX 保留 GID，否则为新 group。
/// - `flags`: 只接受 AT_SYMLINK_NOFOLLOW/AT_EMPTY_PATH。
///
/// # Returns
///
/// 成功为零，失败返回负 errno。
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
    let target = match target(&task, dirfd, name, flags) {
        Ok(target) => target,
        Err(error) => return error,
    };
    chown_inode(&task, target, owner, group)
}
