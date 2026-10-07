//! Linux `inotify_init1`、`inotify_add_watch` 与 `inotify_rm_watch`。

use crate::{
    file::{OpenFileDescription, OpenFileKind},
    fs::{
        IN_CLOEXEC, IN_NONBLOCK, InitError, InodeType, Inotify, WatchError, add_watch_flags, vfs,
    },
    syscall::errno,
    task::current_task,
};

use super::pathname::{ferr, path};

/// 创建 inotify 实例。
///
/// # Parameters
///
/// - `flags`: 只接受 `IN_NONBLOCK` 与 `IN_CLOEXEC`。
///
/// # Returns
///
/// 新 fd；flags 非法 `EINVAL`，实例数已达上限 `EMFILE`，内存或 fd 不足对应 errno。
pub(crate) fn sys_inotify_init1(flags: u32) -> isize {
    if flags & !(IN_NONBLOCK | IN_CLOEXEC) != 0 {
        return -errno::EINVAL;
    }
    let instance = match Inotify::new() {
        Ok(instance) => instance,
        Err(InitError::TooManyInstances) => return -errno::EMFILE,
        Err(InitError::OutOfMemory) => return -errno::ENOMEM,
    };
    let ofd = match OpenFileDescription::anonymous_device(instance, flags & IN_NONBLOCK) {
        Ok(ofd) => ofd,
        Err(error) => return ferr(error),
    };
    current_task()
        .expect("inotify_init1 requires current task")
        .fd_allocate(ofd, flags & IN_CLOEXEC != 0)
        .map_or_else(crate::syscall::file_descriptor_error, |fd| fd as isize)
}

/// 在 `fd` 指向的 inotify 实例上为 `path` 添加或更新 watch。
///
/// # Returns
///
/// watch descriptor（同一实例对同一 inode 重复添加返回原值）。fd 无效 `EBADF`；fd 不是 inotify、事件位
/// 为空或含未知位 `EINVAL`；调用者对目标没有读权限 `EACCES`；达到 watch 上限 `ENOSPC`。
pub(crate) fn sys_inotify_add_watch(fd: usize, pathname: *const u8, mask: u32) -> isize {
    let task = current_task().expect("inotify_add_watch requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let OpenFileKind::Device(file) = &ofd.kind else {
        return -errno::EINVAL;
    };
    let Some(instance) = file.inotify() else {
        return -errno::EINVAL;
    };
    let path = match path(&task, pathname) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let (no_follow, only_directory) = add_watch_flags(mask);
    let identity = task.access_identity(true);
    let start = (path.first() != Some(&b'/')).then(|| task.working_directory());
    let opened = if no_follow {
        vfs().open_file_at_no_follow(start, &path, &identity)
    } else {
        vfs().open_file_at(start, &path, &identity)
    };
    let opened = match opened {
        Ok(opened) => opened,
        Err(error) => return ferr(error),
    };
    let inode = opened.inode();
    if only_directory && inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    let metadata = match inode.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return ferr(error),
    };
    if let Err(error) = identity.require(metadata, 4) {
        return ferr(error);
    }
    match instance.add_watch((inode.filesystem_id(), metadata.inode), mask) {
        Ok(wd) => wd as isize,
        Err(WatchError::InvalidMask) => -errno::EINVAL,
        Err(WatchError::TooManyWatches) => -errno::ENOSPC,
        Err(WatchError::OutOfMemory) => -errno::ENOMEM,
    }
}

/// 删除 watch；之后队列里会出现该 wd 的 `IN_IGNORED`。wd 不属于该实例 `EINVAL`。
pub(crate) fn sys_inotify_rm_watch(fd: usize, wd: i32) -> isize {
    let task = current_task().expect("inotify_rm_watch requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let instance = match &ofd.kind {
        OpenFileKind::Device(file) => file.inotify(),
        _ => None,
    };
    match instance {
        Some(instance) if instance.remove_watch(wd) => 0,
        _ => -errno::EINVAL,
    }
}
