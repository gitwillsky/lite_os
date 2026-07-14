use super::*;

fn positioned_read(fd: usize, vectors: &[UserIoVec], offset: i64) -> isize {
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
    if vectors.iter().all(|vector| vector.length == 0) {
        return 0;
    }
    let file = match RegularFile::from_inode(inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let mut position = offset as u64;
    let result = read_regular_vectors(&task, &file, &mut position, vectors);
    task.account_read_result(result);
    result
}

/// @description 从 regular-file OFD 的显式 offset 读取，不修改共享 OFD offset。
///
/// @param fd 源 descriptor。
/// @param pointer userspace 输出地址。
/// @param length 最大读取长度。
/// @param offset 非负文件偏移。
/// @return byte count、EOF 零或负 errno。
pub(crate) fn sys_pread64(fd: usize, pointer: usize, length: usize, offset: i64) -> isize {
    positioned_read(
        fd,
        &[UserIoVec {
            base: pointer,
            length,
        }],
        offset,
    )
}

/// @description 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// @param fd 目标 descriptor。
/// @param vectors 按序消费的 userspace buffers。
/// @param offset 非负文件偏移；Linux `O_APPEND` OFD 仍在 inode end 执行写入。
/// @param append_override `pwritev2` 对 OFD O_APPEND 的 operation-local override。
/// @return byte count、partial count 或负 errno。
fn positioned_write(
    fd: usize,
    vectors: &[UserIoVec],
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
    if vectors.iter().all(|vector| vector.length == 0) {
        return 0;
    }
    let file = match RegularFile::from_inode(inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };

    let append = append_override.unwrap_or_else(|| *ofd.flags.lock() & O_APPEND != 0);
    let mut position = offset as u64;
    let writer = match file.begin_write() {
        Ok(writer) => writer,
        Err(error) => return ferr(error),
    };
    let result = write_regular_vectors(&task, &writer, &mut position, vectors, append);
    task.account_write_result(result);
    result
}

/// @description 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// @param fd 目标 descriptor。
/// @param pointer userspace 输入地址。
/// @param length 待写入长度。
/// @param offset 非负文件偏移；Linux legacy pwrite64 仍继承 OFD 的 O_APPEND。
/// @return byte count、partial count 或负 errno。
pub(crate) fn sys_pwrite64(fd: usize, pointer: usize, length: usize, offset: i64) -> isize {
    positioned_write(
        fd,
        &[UserIoVec {
            base: pointer,
            length,
        }],
        offset,
        None,
    )
}

fn positioned_readv(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    let task = current_task().expect("preadv requires current task");
    let (vectors, _) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    positioned_read(fd, &vectors, offset)
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
    positioned_write(fd, &vectors, offset, append_override)
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
