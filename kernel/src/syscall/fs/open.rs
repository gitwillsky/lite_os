use alloc::sync::Arc;

use crate::{
    fs::{
        AccessIdentity, InodeType, O_ACCMODE, O_CLOEXEC, O_RDONLY, O_WRONLY, OpenFileDescription,
        OpenedFile, vfs,
    },
    syscall::errno,
    task::{TaskControlBlock, current_task},
};

use super::pathname::{base, ferr, path};

const O_CREAT: u32 = 0x40;
const O_EXCL: u32 = 0x80;
const O_TRUNC: u32 = 0x200;
const O_DIRECTORY: u32 = 0x10000;

/// 校验 directory/search permission 后原子替换 Process 唯一 cwd identity。
///
/// # Parameters
///
/// - `task`: cwd owner。
/// - `opened`: pathname 或 fd 已解析出的 opened-entry identity。
/// - `identity`: 本次 operation 唯一 effective-credentials snapshot。
///
/// # Returns
///
/// 成功返回 0；失败返回负 errno 且 cwd 保持不变。
///
/// # Errors
///
/// 非目录返回 `ENOTDIR`；metadata 或 search permission 失败返回对应 errno。
fn change_directory(
    task: &TaskControlBlock,
    opened: Arc<OpenedFile>,
    identity: &AccessIdentity,
) -> isize {
    let inode = opened.inode();
    if inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    let metadata = match inode.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return ferr(error),
    };
    if let Err(error) = identity.require(metadata, 1) {
        return ferr(error);
    }
    task.set_working_directory(opened);
    0
}

/// 校验 pathname search permission 后替换 Process cwd opened entry。
pub(crate) fn sys_chdir(name: *const u8) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let start = (path.first() != Some(&b'/')).then(|| task.working_directory());
    let identity = task.access_identity(true);
    let opened = match vfs().open_file_at(start, &path, &identity) {
        Ok(opened) => opened,
        Err(error) => return ferr(error),
    };
    change_directory(&task, opened, &identity)
}

/// 按 Linux `fchdir` 从 live descriptor 的 opened entry 替换 Process cwd。
///
/// # Parameters
///
/// - `fd`: 当前 Process descriptor number。
///
/// # Returns
///
/// 成功返回 0；失败返回负 errno 且 cwd 保持不变。
///
/// # Errors
///
/// - descriptor 不存在返回 `EBADF`；非 pathname-backed 或非目录 fd 返回 `ENOTDIR`。
/// - metadata 或 search permission 失败返回对应 errno。
pub(crate) fn sys_fchdir(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let Some(opened) = ofd.opened_ref() else {
        return -errno::ENOTDIR;
    };
    let identity = task.access_identity(true);
    change_directory(&task, opened, &identity)
}

/// 以 effective credentials 执行 open/create permission 并发布 OFD。
pub(crate) fn sys_openat(fd: isize, name: *const u8, flags: u32, mode: u32) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let path = match path(&task, name) {
        Ok(path) => path,
        Err(error) => return error,
    };
    if flags & O_ACCMODE == O_ACCMODE {
        return -errno::EINVAL;
    }
    let start = match base(&task, fd, &path) {
        Ok(start) => start,
        Err(error) => return error,
    };
    let identity = task.access_identity(true);
    let opened = if flags & O_CREAT != 0 {
        match vfs().open_or_create_file_at(
            start,
            &path,
            task.creation_mode(mode),
            &identity,
            flags & O_EXCL != 0,
        ) {
            Ok(opened) => opened,
            Err(error) => return ferr(error),
        }
    } else {
        match vfs().open_file_at(start, &path, &identity) {
            Ok(opened) => opened,
            Err(error) => return ferr(error),
        }
    };
    let inode = opened.inode();
    let requested = match flags & O_ACCMODE {
        O_RDONLY => 4,
        O_WRONLY => 2,
        _ => 6,
    };
    let metadata = match inode.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return ferr(error),
    };
    if let Err(error) = identity.require(metadata, requested) {
        return ferr(error);
    }
    if flags & O_DIRECTORY != 0 && inode.inode_type() != InodeType::Directory {
        return -errno::ENOTDIR;
    }
    if inode.inode_type() == InodeType::Directory && flags & O_ACCMODE != O_RDONLY {
        return -errno::EISDIR;
    }
    if !matches!(
        inode.inode_type(),
        InodeType::File | InodeType::Directory | InodeType::CharacterDevice
    ) || inode.inode_type() == InodeType::CharacterDevice && inode.device_number().is_none()
    {
        return -errno::ENXIO;
    }
    let ofd_flags = flags & !(O_CREAT | O_EXCL | O_TRUNC | O_CLOEXEC);
    let ofd = if let Some(number) = inode.device_number() {
        let request = crate::fs::device::OpenRequest {
            number,
            identity: &identity,
        };
        match crate::fs::device::open(&request)
            .and_then(|file| OpenFileDescription::device(file, ofd_flags, opened))
        {
            Ok(ofd) => ofd,
            Err(error) => return ferr(error),
        }
    } else {
        let ofd = match OpenFileDescription::inode(opened, ofd_flags) {
            Ok(ofd) => ofd,
            Err(()) => return -errno::ENOMEM,
        };
        if flags & O_TRUNC != 0
            && flags & O_ACCMODE != O_RDONLY
            && let Err(error) = crate::fs::truncate(inode.clone(), 0)
        {
            return ferr(error);
        }
        ofd
    };
    task.fd_allocate(ofd, flags & O_CLOEXEC != 0)
        .map_or_else(super::super::file_descriptor_error, |fd| fd as isize)
}
