use super::*;

fn positioned_read(fd: usize, vectors: &[UserIoVec], offset: i64) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pread64 requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if ofd.status_flags() & O_ACCMODE == O_WRONLY {
        return -errno::EBADF;
    }
    // 路径打开的文件与 memfd 都有 inode 内容；pipe、socket 与字符设备没有可定位的内容。
    let Some(inode) = ofd.inode_ref() else {
        return -errno::ESPIPE;
    };
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
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
    if result > 0
        && let Some(opened) = ofd.opened_ref()
    {
        crate::fs::notify_opened(&opened, crate::fs::IN_ACCESS);
    }
    result
}

/// 从 regular-file OFD 的显式 offset 读取，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `pointer`: userspace 输出地址。
/// - `length`: 最大读取长度。
/// - `offset`: 非负文件偏移。
///
/// # Returns
///
/// byte count、EOF 零或负 errno。
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

/// 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `vectors`: 按序消费的 userspace buffers。
/// - `offset`: 非负文件偏移；Linux `O_APPEND` OFD 仍在 inode end 执行写入。
/// - `append_override`: `pwritev2` 对 OFD O_APPEND 的 operation-local override。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
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
    if ofd.status_flags() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    // 路径打开的文件与 memfd 都有 inode 内容；pipe、socket 与字符设备没有可定位的内容。
    let Some(inode) = ofd.inode_ref() else {
        return -errno::ESPIPE;
    };
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return -errno::ESPIPE;
    }
    if vectors.iter().all(|vector| vector.length == 0) {
        return 0;
    }
    let total_length = vectors
        .iter()
        .try_fold(0usize, |total, vector| total.checked_add(vector.length))
        .expect("positioned write vectors must have a checked total length");
    let file = match RegularFile::from_inode(inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };

    let append = append_override.unwrap_or_else(|| ofd.status_flags() & O_APPEND != 0);
    let staging = PreparedRegularWriteStaging::prepare(total_length);
    let result = with_prepared_staging(staging, |staging| {
        let mut staging = staging.as_input_staging();
        let mut position = offset as u64;
        let writer = match file.begin_write() {
            Ok(writer) => writer,
            Err(error) => return ferr(error),
        };
        write_regular_vectors(&task, &writer, &mut position, vectors, append, &mut staging)
    });
    task.account_write_result(result);
    if result > 0
        && let Some(opened) = ofd.opened_ref()
    {
        crate::fs::notify_opened(&opened, crate::fs::IN_MODIFY);
    }
    result
}

/// 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `pointer`: userspace 输入地址。
/// - `length`: 待写入长度。
/// - `offset`: 非负文件偏移；Linux legacy pwrite64 仍继承 OFD 的 O_APPEND。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
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

/// 按 Linux preadv ABI 从显式 offset scatter read，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 非负显式 offset。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_preadv(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    positioned_readv(fd, iovector, count, offset)
}

/// 按 Linux pwritev ABI 向显式 offset gather write，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 非负显式 offset。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_pwritev(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    positioned_writev(fd, iovector, count, offset, None)
}

/// 实现 Linux preadv2；offset=-1 使用共享 OFD offset，其余为 positioned read。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 显式 offset 或 -1。
/// - `flags`: 当前同步 VFS 不支持异步/缓存 hint，非零 flags 返回 EOPNOTSUPP。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
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

/// 实现 Linux pwritev2 的 append override 与同步写语义。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 显式 offset 或 -1。
/// - `flags`: 支持 RWF_DSYNC/RWF_SYNC/RWF_APPEND/RWF_NOAPPEND；其他 flags 返回 EOPNOTSUPP。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
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
