//! asm-generic xattr ABI；路径查找交给现有VFS，权限与属性变更交给实际OpenedFile。
use super::pathname::{base, ferr, path_allow_empty};
use crate::{
    fs::{FileSystemError, OpenedFile, XattrError, vfs},
    memory::UserAccessError,
    syscall::errno,
    task::{TaskControlBlock, current_task},
};
use alloc::{sync::Arc, vec::Vec};

pub(crate) const EMPTY_PATH: u32 = 0x1000;
pub(crate) const NOFOLLOW: u32 = 0x100;
const MAX_SIZE: usize = 65_536;

fn user_error(error: UserAccessError) -> isize {
    match error {
        UserAccessError::OutOfMemory => -errno::ENOMEM,
        UserAccessError::Unterminated => -errno::ERANGE,
        UserAccessError::Fault | UserAccessError::Overflow => -errno::EFAULT,
    }
}

fn xattr_error(error: XattrError) -> isize {
    match error {
        XattrError::Filesystem(FileSystemError::NotFound) => -errno::ENODATA,
        XattrError::Filesystem(other) => ferr(other),
        XattrError::Overflow => -errno::EOVERFLOW,
    }
}

/// Linux get/list 在最大缓冲仍装不下时用 E2BIG；较小 caller 缓冲保持 ERANGE。
fn read_error(error: XattrError, size: usize) -> isize {
    let error = xattr_error(error);
    if error == -errno::ERANGE && size >= MAX_SIZE {
        -errno::E2BIG
    } else {
        error
    }
}

fn name(task: &TaskControlBlock, pointer: usize) -> Result<Vec<u8>, isize> {
    let name = task.copy_user_c_string(pointer, 256).map_err(user_error)?;
    if name.is_empty() {
        return Err(-errno::ERANGE);
    }
    Ok(name)
}

fn target(
    task: &TaskControlBlock,
    dirfd: isize,
    pathname: usize,
    flags: u32,
) -> Result<Arc<OpenedFile>, isize> {
    let path = if pathname == 0 && flags & EMPTY_PATH != 0 {
        Vec::new()
    } else {
        path_allow_empty(task, pathname as *const u8)?
    };
    if path.is_empty() {
        if flags & EMPTY_PATH == 0 {
            return Err(-errno::ENOENT);
        }
        return usize::try_from(dirfd)
            .ok()
            .and_then(|fd| task.fd_get(fd))
            .map(|ofd| ofd.opened_ref())
            .ok_or(-errno::EBADF);
    }
    let origin = base(task, dirfd, &path)?;
    let identity = task.access_identity(true);
    if flags & NOFOLLOW != 0 {
        vfs().open_file_at_no_follow(origin, &path, &identity)
    } else {
        vfs().open_file_at(origin, &path, &identity)
    }
    .map_err(ferr)
}

fn at_flags(flags: u32) -> Result<(), isize> {
    if flags & !(EMPTY_PATH | NOFOLLOW) != 0 {
        return Err(-errno::EINVAL);
    }
    Ok(())
}

/// setxattr族共用入口；按规范先copy名称和值，再查找path/fd，零size设置实际空属性。
pub(crate) fn set(
    dirfd: isize,
    pathname: usize,
    lookup: u32,
    attribute: usize,
    value: usize,
    size: usize,
    flags: u32,
) -> isize {
    let task = current_task().expect("setxattr requires current task");
    let result = (|| {
        at_flags(lookup)?;
        if flags & !3 != 0 {
            return Err(-errno::EINVAL);
        }
        let name = name(&task, attribute)?;
        if size > MAX_SIZE {
            return Err(-errno::E2BIG);
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(size).map_err(|_| -errno::ENOMEM)?;
        bytes.resize(size, 0);
        if size != 0 {
            task.copy_from_user(value, &mut bytes).map_err(user_error)?;
        }
        let opened = target(&task, dirfd, pathname, lookup)?;
        opened
            .change_xattr(&name, Some(bytes), flags, &task.access_identity(true))
            .map_err(xattr_error)
    })();
    result.map_or_else(|error| error, |()| 0)
}

/// get/list的非零缓冲截到Linux64KiB上限，只有完整快照才copyout；零缓冲由独立长度查询处理。
fn output(task: &TaskControlBlock, bytes: &[u8], pointer: usize, size: usize) -> isize {
    let capacity = size.min(MAX_SIZE);
    if bytes.len() > capacity {
        return if capacity == MAX_SIZE {
            -errno::E2BIG
        } else {
            -errno::ERANGE
        };
    }
    if !bytes.is_empty()
        && let Err(error) = task.copy_to_user(pointer, bytes)
    {
        return user_error(error);
    }
    bytes.len() as isize
}

/// getxattr族读取caller-visible属性；pathname错误在属性域错误映射之前返回。
pub(crate) fn get(
    dirfd: isize,
    pathname: usize,
    lookup: u32,
    attribute: usize,
    value: usize,
    size: usize,
) -> isize {
    let task = current_task().expect("getxattr requires current task");
    let result = (|| {
        at_flags(lookup)?;
        let name = name(&task, attribute)?;
        let opened = target(&task, dirfd, pathname, lookup)?;
        if size == 0 {
            return opened
                .xattr_size(&name, &task.access_identity(true))
                .map(|length| length as isize)
                .map_err(xattr_error);
        }
        let bytes = opened
            .read_xattr(&name, &task.access_identity(true))
            .map_err(|error| read_error(error, size))?;
        Ok(output(&task, &bytes, value, size))
    })();
    result.unwrap_or_else(|error| error)
}

/// listxattr族读取NUL分隔名称；名称可见性由实际filesystem判定。
pub(crate) fn list(dirfd: isize, pathname: usize, lookup: u32, value: usize, size: usize) -> isize {
    let task = current_task().expect("listxattr requires current task");
    let result = (|| {
        at_flags(lookup)?;
        let opened = target(&task, dirfd, pathname, lookup)?;
        if size == 0 {
            return opened
                .xattr_list_size(&task.access_identity(true))
                .map(|length| length as isize)
                .map_err(xattr_error);
        }
        let bytes = opened
            .list_xattrs(&task.access_identity(true))
            .map_err(|error| read_error(error, size))?;
        Ok(output(&task, &bytes, value, size))
    })();
    result.unwrap_or_else(|error| error)
}

/// removexattr必须存在；统一实际writer/权限路径，不把missing属性作为成功删除。
pub(crate) fn remove(dirfd: isize, pathname: usize, lookup: u32, attribute: usize) -> isize {
    let task = current_task().expect("removexattr requires current task");
    let result = (|| {
        at_flags(lookup)?;
        let name = name(&task, attribute)?;
        let opened = target(&task, dirfd, pathname, lookup)?;
        opened
            .change_xattr(&name, None, 2, &task.access_identity(true))
            .map_err(xattr_error)
    })();
    result.map_or_else(|error| error, |()| 0)
}

/// 新版*xattrat的copy_struct_from_user；固定16-byte wire布局，扩展区必须为零。
/// 不把用户指针强转为Rust结构体引用；size上限和扩展字段错误遵循LinuxABI。
fn arguments(pointer: usize, size: usize) -> Result<(usize, usize, u32), isize> {
    if size < 16 {
        return Err(-errno::EINVAL);
    }
    if size > 4096 {
        return Err(-errno::E2BIG);
    }
    let task = current_task().expect("xattrat requires current task");
    let mut wire = [0u8; 16];
    task.copy_from_user(pointer, &mut wire)
        .map_err(user_error)?;
    let mut extension = [0u8; 64];
    let mut offset = 16;
    while offset < size {
        let length = (size - offset).min(extension.len());
        let address = pointer.checked_add(offset).ok_or(-errno::EFAULT)?;
        task.copy_from_user(address, &mut extension[..length])
            .map_err(user_error)?;
        if extension[..length].iter().any(|byte| *byte != 0) {
            return Err(-errno::E2BIG);
        }
        offset += length;
    }
    Ok((
        u64::from_ne_bytes(wire[..8].try_into().expect("value wire")) as usize,
        u32::from_ne_bytes(wire[8..12].try_into().expect("size wire")) as usize,
        u32::from_ne_bytes(wire[12..].try_into().expect("flags wire")),
    ))
}

/// setxattrat解码标准扩展参数后进入唯一set路径。
pub(crate) fn set_at(args: [usize; 6]) -> isize {
    match arguments(args[4], args[5]) {
        Ok((value, size, flags)) => set(
            args[0] as i32 as isize,
            args[1],
            args[2] as u32,
            args[3],
            value,
            size,
            flags,
        ),
        Err(error) => error,
    }
}
/// getxattrat只接受零操作flags；路径flags由共用get路径检查。
pub(crate) fn get_at(args: [usize; 6]) -> isize {
    match arguments(args[4], args[5]) {
        Ok((_, _, flags)) if flags != 0 => -errno::EINVAL,
        Ok((value, size, _)) => get(
            args[0] as i32 as isize,
            args[1],
            args[2] as u32,
            args[3],
            value,
            size,
        ),
        Err(error) => error,
    }
}
