use alloc::{sync::Arc, vec::Vec};

use crate::{
    fs::{FileSystemError, Inode, InodeType},
    memory::UserAccessError,
    syscall::errno,
    task::TaskControlBlock,
};

use super::AT_FDCWD;

pub(super) fn ferr(error: FileSystemError) -> isize {
    -(match error {
        FileSystemError::NotFound => errno::ENOENT,
        FileSystemError::AlreadyExists => errno::EEXIST,
        FileSystemError::NotDirectory => errno::ENOTDIR,
        FileSystemError::IsDirectory => errno::EISDIR,
        FileSystemError::DirectoryNotEmpty => errno::ENOTEMPTY,
        FileSystemError::NoSpace => errno::ENOSPC,
        FileSystemError::CrossDevice => errno::EXDEV,
        FileSystemError::PermissionDenied => errno::EPERM,
        FileSystemError::AccessDenied => errno::EACCES,
        FileSystemError::TooManyLinks => errno::EMLINK,
        FileSystemError::InvalidPath | FileSystemError::InvalidOperation => errno::EINVAL,
        FileSystemError::ReadOnly => errno::EROFS,
        FileSystemError::SymbolicLink => errno::ELOOP,
        FileSystemError::OutOfMemory => errno::ENOMEM,
        FileSystemError::IoError | FileSystemError::InvalidFileSystem => errno::EIO,
    })
}

pub(super) fn path(task: &TaskControlBlock, pointer: *const u8) -> Result<Vec<u8>, isize> {
    let path = path_allow_empty(task, pointer)?;
    if path.is_empty() {
        return Err(-errno::ENOENT);
    }
    Ok(path)
}

pub(super) fn path_allow_empty(
    task: &TaskControlBlock,
    pointer: *const u8,
) -> Result<Vec<u8>, isize> {
    if pointer.is_null() {
        return Err(-errno::EFAULT);
    }
    let path = task
        .copy_user_c_string(pointer as usize, 4096)
        .map_err(|error| match error {
            UserAccessError::Unterminated => -errno::ENAMETOOLONG,
            UserAccessError::OutOfMemory => -errno::ENOMEM,
            UserAccessError::Fault | UserAccessError::Overflow => -errno::EFAULT,
        })?;
    Ok(path)
}

pub(super) fn base(
    task: &TaskControlBlock,
    fd: isize,
    path: &[u8],
) -> Result<Option<Arc<dyn Inode>>, isize> {
    if path.first() == Some(&b'/') {
        return Ok(None);
    }
    if fd == AT_FDCWD {
        return Ok(Some(task.working_directory()));
    }
    let ofd = task.fd_get(fd as usize).ok_or(-errno::EBADF)?;
    let inode = ofd.inode_ref().ok_or(-errno::ENOTDIR)?;
    if inode.inode_type() != InodeType::Directory {
        return Err(-errno::ENOTDIR);
    }
    Ok(Some(inode))
}
