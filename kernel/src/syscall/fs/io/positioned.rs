use super::*;

/// @description 从 regular-file OFD 的显式 offset 读取，不修改共享 OFD offset。
///
/// @param fd 源 descriptor。
/// @param pointer userspace 输出地址。
/// @param length 最大读取长度。
/// @param offset 非负文件偏移。
/// @return byte count、EOF 零或负 errno。
pub(crate) fn sys_pread64(fd: usize, pointer: usize, length: usize, offset: i64) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pread64 requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_WRONLY {
        return -errno::EBADF;
    }
    let OpenFileKind::Inode(opened) = &ofd.kind else {
        return -errno::ESPIPE;
    };
    let inode = opened.inode();
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if inode.inode_type() != InodeType::File {
        return -errno::ESPIPE;
    }

    let mut position = offset as u64;
    let mut total = 0;
    let mut chunk = [0u8; 512];
    while total < length {
        let count = chunk.len().min(length - total);
        let read = match crate::fs::read(inode.clone(), position, &mut chunk[..count]) {
            Ok(read) => read,
            Err(error) => {
                return if total == 0 {
                    ferr(error)
                } else {
                    total as isize
                };
            }
        };
        if read == 0 {
            break;
        }
        let Some(address) = pointer.checked_add(total) else {
            return if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        };
        if task.copy_to_user(address, &chunk[..read]).is_err() {
            return if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        }
        position += read as u64;
        total += read;
    }
    total as isize
}

/// @description 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// @param fd 目标 descriptor。
/// @param pointer userspace 输入地址。
/// @param length 待写入长度。
/// @param offset 非负文件偏移；Linux `O_APPEND` OFD 仍在 inode end 执行写入。
/// @return byte count、partial count 或负 errno。
fn positioned_write(
    fd: usize,
    pointer: usize,
    length: usize,
    offset: i64,
    append_override: Option<bool>,
) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pwrite64 requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    let OpenFileKind::Inode(opened) = &ofd.kind else {
        return -errno::ESPIPE;
    };
    let inode = opened.inode();
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if inode.inode_type() != InodeType::File {
        return -errno::ESPIPE;
    }

    let append = append_override.unwrap_or_else(|| *ofd.flags.lock() & O_APPEND != 0);
    let mut position = offset as u64;
    let mut total = 0;
    let mut chunk = [0u8; 512];
    while total < length {
        let requested = chunk.len().min(length - total);
        let count = match bounded_regular_write(&task, &ofd, position, requested, total) {
            Ok(count) => count,
            Err(result) => return result,
        };
        let Some(address) = pointer.checked_add(total) else {
            return if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        };
        if task.copy_from_user(address, &mut chunk[..count]).is_err() {
            return if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        }
        let written = if append {
            match crate::fs::append(inode.clone(), &chunk[..count], task.file_size_limit()) {
                Ok((_, 0)) if count != 0 => {
                    return if total == 0 {
                        file_size_exceeded(&task)
                    } else {
                        total as isize
                    };
                }
                Ok((_, written)) => written,
                Err(error) => {
                    return if total == 0 {
                        ferr(error)
                    } else {
                        total as isize
                    };
                }
            }
        } else {
            match crate::fs::write(inode.clone(), position, &chunk[..count]) {
                Ok(written) => written,
                Err(error) => {
                    return if total == 0 {
                        ferr(error)
                    } else {
                        total as isize
                    };
                }
            }
        };
        position += written as u64;
        total += written;
        if written < count {
            break;
        }
    }
    total as isize
}

/// @description 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// @param fd 目标 descriptor。
/// @param pointer userspace 输入地址。
/// @param length 待写入长度。
/// @param offset 非负文件偏移；Linux legacy pwrite64 仍继承 OFD 的 O_APPEND。
/// @return byte count、partial count 或负 errno。
pub(crate) fn sys_pwrite64(fd: usize, pointer: usize, length: usize, offset: i64) -> isize {
    positioned_write(fd, pointer, length, offset, None)
}

fn positioned_readv(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    let task = current_task().expect("preadv requires current task");
    let (vectors, _) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if vectors.is_empty() {
        return sys_pread64(fd, 0, 0, offset);
    }
    let mut position = offset;
    let mut total = 0usize;
    for vector in vectors {
        let result = sys_pread64(fd, vector.base, vector.length, position);
        if result < 0 {
            return if total == 0 { result } else { total as isize };
        }
        let result = result as usize;
        total += result;
        position = match position.checked_add(result as i64) {
            Some(position) => position,
            None => return total as isize,
        };
        if result < vector.length {
            break;
        }
    }
    total as isize
}

fn positioned_writev(
    fd: usize,
    iovector: usize,
    count: usize,
    offset: i64,
    append_override: Option<bool>,
) -> isize {
    let task = current_task().expect("pwritev requires current task");
    let (vectors, _) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if vectors.is_empty() {
        return positioned_write(fd, 0, 0, offset, append_override);
    }
    let mut position = offset;
    let mut total = 0usize;
    for vector in vectors {
        let result = positioned_write(fd, vector.base, vector.length, position, append_override);
        if result < 0 {
            return if total == 0 { result } else { total as isize };
        }
        let result = result as usize;
        total += result;
        position = match position.checked_add(result as i64) {
            Some(position) => position,
            None => return total as isize,
        };
        if result < vector.length {
            break;
        }
    }
    total as isize
}

/// @description 按 Linux preadv ABI 从显式 offset scatter read，不修改共享 OFD offset。
/// @param fd 源 descriptor。
/// @param iovector userspace iovec 数组。
/// @param count iovec 数量。
/// @param offset 非负显式 offset。
/// @return byte count、partial count 或负 errno。
pub(crate) fn sys_preadv(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    positioned_readv(fd, iovector, count, offset)
}

/// @description 按 Linux pwritev ABI 向显式 offset gather write，不修改共享 OFD offset。
/// @param fd 目标 descriptor。
/// @param iovector userspace iovec 数组。
/// @param count iovec 数量。
/// @param offset 非负显式 offset。
/// @return byte count、partial count 或负 errno。
pub(crate) fn sys_pwritev(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    positioned_writev(fd, iovector, count, offset, None)
}

/// @description 实现 Linux preadv2；offset=-1 使用共享 OFD offset，其余为 positioned read。
/// @param fd 源 descriptor。
/// @param iovector userspace iovec 数组。
/// @param count iovec 数量。
/// @param offset 显式 offset 或 -1。
/// @param flags 当前同步 VFS 不支持异步/缓存 hint，非零 flags 返回 EOPNOTSUPP。
/// @return byte count、partial count 或负 errno。
pub(crate) fn sys_preadv2(
    fd: usize,
    iovector: usize,
    count: usize,
    offset: i64,
    flags: u32,
) -> isize {
    if flags != 0 {
        return -errno::EOPNOTSUPP;
    }
    if offset == -1 {
        return sys_readv(fd, iovector, count);
    }
    positioned_readv(fd, iovector, count, offset)
}

const RWF_DSYNC: u32 = 0x02;
const RWF_SYNC: u32 = 0x04;
const RWF_APPEND: u32 = 0x10;
const RWF_NOAPPEND: u32 = 0x20;
const SUPPORTED_WRITE_FLAGS: u32 = RWF_DSYNC | RWF_SYNC | RWF_APPEND | RWF_NOAPPEND;

/// @description 实现 Linux pwritev2 的 append override 与同步写语义。
/// @param fd 目标 descriptor。
/// @param iovector userspace iovec 数组。
/// @param count iovec 数量。
/// @param offset 显式 offset 或 -1。
/// @param flags 支持 RWF_DSYNC/RWF_SYNC/RWF_APPEND/RWF_NOAPPEND；其他 flags 返回 EOPNOTSUPP。
/// @return byte count、partial count 或负 errno。
pub(crate) fn sys_pwritev2(
    fd: usize,
    iovector: usize,
    count: usize,
    offset: i64,
    flags: u32,
) -> isize {
    if flags & !SUPPORTED_WRITE_FLAGS != 0 {
        return -errno::EOPNOTSUPP;
    }
    if flags & RWF_APPEND != 0 && flags & RWF_NOAPPEND != 0 {
        return -errno::EINVAL;
    }
    if offset == -1 {
        if flags & (RWF_APPEND | RWF_NOAPPEND) != 0 {
            return -errno::EOPNOTSUPP;
        }
        let result = sys_writev(fd, iovector, count);
        if result >= 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
            let sync = super::super::sync_file(fd);
            if sync < 0 {
                return sync;
            }
        }
        return result;
    }
    let append_override = if flags & RWF_APPEND != 0 {
        Some(true)
    } else if flags & RWF_NOAPPEND != 0 {
        Some(false)
    } else {
        None
    };
    let result = positioned_writev(fd, iovector, count, offset, append_override);
    if result >= 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        let sync = super::super::sync_file(fd);
        if sync < 0 {
            return sync;
        }
    }
    result
}
