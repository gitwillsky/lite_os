use super::*;

mod read;
use read::read_descriptor;
mod write;
use write::write_descriptor;

/// @description 把 task-layer pipe wait result 统一翻译为 syscall control flow。
/// @param pipe anonymous pipe owner。
/// @param condition blocking I/O 必须满足的精确 read/write 条件。
/// @return ready 返回 Ok；signal interruption 返回 `-EINTR`。
fn block_on_pipe(pipe: &Arc<Pipe>, condition: PipeWaitCondition) -> Result<(), isize> {
    match wait_for_pipe(pipe, condition) {
        WaitResult::Woken => Ok(()),
        WaitResult::Interrupted => Err(-errno::EINTR),
        WaitResult::TimedOut => panic!("pipe I/O wait cannot time out"),
    }
}

/// @description 取得已证明可读且实现 read file operation 的 OFD。
/// @param fd caller descriptor number。
/// @return 当前 task 与共享 OFD；access/capability 检查先于任何 userspace iovec import。
/// @error 无当前 task、fd 不存在、OFD 只写或 backend 不提供 read 时返回标准 errno。
fn readable_descriptor(
    fd: usize,
) -> Result<(Arc<TaskControlBlock>, Arc<OpenFileDescription>), isize> {
    let task = current_task().ok_or(-errno::ESRCH)?;
    let ofd = task.fd_get(fd).ok_or(-errno::EBADF)?;
    if *ofd.flags.lock() & O_ACCMODE == O_WRONLY {
        return Err(-errno::EBADF);
    }
    if matches!(&ofd.kind, OpenFileKind::Epoll(_)) {
        return Err(-errno::EINVAL);
    }
    Ok((task, ofd))
}

/// @description 取得已证明可写且实现 write file operation 的 OFD。
/// @param fd caller descriptor number。
/// @return 当前 task 与共享 OFD；access/capability 检查先于任何 userspace iovec import。
/// @error 无当前 task、fd 不存在、OFD 只读或 backend 不提供 write 时返回标准 errno。
fn writable_descriptor(
    fd: usize,
) -> Result<(Arc<TaskControlBlock>, Arc<OpenFileDescription>), isize> {
    let task = current_task().ok_or(-errno::ESRCH)?;
    let ofd = task.fd_get(fd).ok_or(-errno::EBADF)?;
    if *ofd.flags.lock() & O_ACCMODE == O_RDONLY {
        return Err(-errno::EBADF);
    }
    if matches!(&ofd.kind, OpenFileKind::Epoll(_)) {
        return Err(-errno::EINVAL);
    }
    Ok((task, ofd))
}

/// @description 为一次 non-regular I/O 分配精确长度的连续 kernel buffer。
/// @param length buffer byte count。
/// @return 已清零且长度等于 length 的 buffer。
/// @error allocator 无法满足请求时返回 `ENOMEM`。
fn buffer(length: usize) -> Result<Vec<u8>, isize> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| -errno::ENOMEM)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

/// @description 将 scatter copy 结果翻译为 Linux partial-count/EFAULT 语义。
/// @param cursor 本次 copyout 的唯一 progress owner。
/// @param result copyout 结果。
/// @return 全部 byte count、已有进度的 partial count，或首字节失败的 `EFAULT`。
fn scatter_result(cursor: &UserIoCursor<'_>, result: Result<usize, ()>) -> isize {
    match result {
        Ok(copied) => copied as isize,
        Err(()) if cursor.completed() == 0 => -errno::EFAULT,
        Err(()) => cursor.completed() as isize,
    }
}

/// @description 从 descriptor 读取至单一 userspace buffer。
/// @param fd 源 descriptor。
/// @param pointer userspace 输出地址。
/// @param length 最大读取长度。
/// @return byte count、EOF 零或负 errno/internal restart sentinel。
pub(crate) fn sys_read(fd: usize, pointer: *mut u8, length: usize) -> isize {
    let (task, ofd) = match readable_descriptor(fd) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let result = read_descriptor(
        &task,
        &ofd,
        &[UserIoVec {
            base: pointer as usize,
            length,
        }],
        length,
    );
    task.account_read_result(result);
    result
}

/// @description 按 Linux RV64 `struct iovec` 顺序从同一个 OFD scatter read。
/// @param fd 源 descriptor。
/// @param iovector userspace `iovec` 数组地址；count 为零时可为空。
/// @param count iovec 数量，最大 1024。
/// @return 总读取字节数；导入失败或首个 read 失败返回负 errno，已有进度后返回 partial count。
pub(crate) fn sys_readv(fd: usize, iovector: usize, count: usize) -> isize {
    let (task, ofd) = match readable_descriptor(fd) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let (vectors, total_length) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let result = read_descriptor(&task, &ofd, &vectors, total_length);
    task.account_read_result(result);
    result
}

/// @description 将单一 userspace buffer 写入 descriptor。
/// @param fd 目标 descriptor。
/// @param pointer userspace 输入地址。
/// @param length 待写入长度。
/// @return byte count、partial count 或负 errno/internal restart sentinel。
pub(crate) fn sys_write(fd: usize, pointer: *const u8, length: usize) -> isize {
    let (task, ofd) = match writable_descriptor(fd) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let result = write_descriptor(
        &task,
        &ofd,
        &[UserIoVec {
            base: pointer as usize,
            length,
        }],
        length,
    );
    task.account_write_result(result);
    result
}

/// @description 按 Linux RV64 `struct iovec` 顺序写入同一个 open file description。
/// @param fd 目标 descriptor。
/// @param iovector userspace `iovec` 数组地址；count 为零时可为空。
/// @param count iovec 数量，最大 1024。
/// @return 总写入字节数；导入失败或首个 write 失败返回负 errno，已有进度后返回 partial count。
pub(crate) fn sys_writev(fd: usize, iovector: usize, count: usize) -> isize {
    let (task, ofd) = match writable_descriptor(fd) {
        Ok(context) => context,
        Err(error) => return error,
    };
    let (vectors, total_length) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let result = write_descriptor(&task, &ofd, &vectors, total_length);
    task.account_write_result(result);
    result
}
