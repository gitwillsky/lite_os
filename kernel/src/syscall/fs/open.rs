use crate::{
    fs::{
        DeviceKind, FileSystemError, InodeType, O_ACCMODE, O_CLOEXEC, O_RDONLY, O_WRONLY,
        OpenFileDescription, vfs,
    },
    syscall::errno,
    task::{current_task, session_id},
};

use super::pathname::{base, ferr, path};

const O_CREAT: u32 = 0x40;
const O_EXCL: u32 = 0x80;
const O_TRUNC: u32 = 0x200;
const O_DIRECTORY: u32 = 0x10000;

/// @description 校验 pathname search permission 后替换 Process cwd opened entry。
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

/// @description 以 effective credentials 执行 open/create permission 并发布 OFD。
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
    let opened = match vfs().open_file_at(start.clone(), &path, &identity) {
        Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return -errno::EEXIST,
        Ok(opened) => opened,
        Err(FileSystemError::NotFound) if flags & O_CREAT != 0 => {
            if path.last() == Some(&b'/') {
                return -errno::ENOTDIR;
            }
            match vfs().create_at(
                start,
                &path,
                InodeType::File,
                task.creation_mode(mode),
                &identity,
            ) {
                Ok(opened) => opened,
                Err(error) => return ferr(error),
            }
        }
        Err(error) => return ferr(error),
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
    ) || inode.inode_type() == InodeType::CharacterDevice && inode.device_kind().is_none()
    {
        return -errno::ENXIO;
    }
    let ofd_flags = flags & !(O_CREAT | O_EXCL | O_TRUNC | O_CLOEXEC);
    let ofd = if let Some(device) = inode.device_kind() {
        let terminal = task.terminal();
        if device == DeviceKind::Tty {
            let Ok(session) = session_id(0) else {
                return -errno::ENXIO;
            };
            if terminal.controlling_session() != Some(session) {
                return -errno::ENXIO;
            }
        }
        OpenFileDescription::character(device, terminal, ofd_flags, opened)
    } else {
        if flags & O_TRUNC != 0
            && flags & O_ACCMODE != O_RDONLY
            && let Err(error) = crate::fs::truncate(inode.clone(), 0)
        {
            return ferr(error);
        }
        OpenFileDescription::inode(opened, ofd_flags)
    };
    task.fd_allocate(ofd, flags & O_CLOEXEC != 0)
        .map_or(-errno::EMFILE, |fd| fd as isize)
}
