use super::*;

mod read;
use read::read_descriptor;
mod write;
use write::write_descriptor;

/// 把 task-layer pipe wait result 统一翻译为 syscall control flow。
///
/// # Parameters
///
/// - `pipe`: anonymous pipe owner。
/// - `condition`: blocking I/O 必须满足的精确 read/write 条件。
///
/// # Returns
///
/// ready 返回 Ok；signal interruption 返回 `-EINTR`。
fn block_on_pipe(pipe: &Arc<Pipe>, condition: PipeWaitCondition) -> Result<(), isize> {
    match wait_for_pipe(pipe, condition) {
        WaitResult::Woken => Ok(()),
        WaitResult::Interrupted => Err(-errno::EINTR),
        WaitResult::TimedOut => panic!("pipe I/O wait cannot time out"),
        WaitResult::OutOfMemory => Err(-errno::ENOMEM),
    }
}

/// 取得已证明可读且实现 read file operation 的 OFD。
///
/// # Parameters
///
/// - `fd`: caller descriptor number。
///
/// # Returns
///
/// 当前 task 与共享 OFD；access/capability 检查先于任何 userspace iovec import。
///
/// # Errors
///
/// 无当前 task、fd 不存在、OFD 只写或 backend 不提供 read 时返回标准 errno。
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

/// 取得已证明可写且实现 write file operation 的 OFD。
///
/// # Parameters
///
/// - `fd`: caller descriptor number。
///
/// # Returns
///
/// 当前 task 与共享 OFD；access/capability 检查先于任何 userspace iovec import。
///
/// # Errors
///
/// 无当前 task、fd 不存在、OFD 只读或 backend 不提供 write 时返回标准 errno。
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

/// 将 scatter copy 结果翻译为 Linux partial-count/EFAULT 语义。
///
/// # Parameters
///
/// - `cursor`: 本次 copyout 的唯一 progress owner。
/// - `result`: copyout 结果。
///
/// # Returns
///
/// 全部 byte count、已有进度的 partial count，或首字节失败的 `EFAULT`。
fn scatter_result(cursor: &UserIoCursor<'_>, result: Result<usize, ()>) -> isize {
    match result {
        Ok(copied) => copied as isize,
        Err(()) if cursor.completed() == 0 => -errno::EFAULT,
        Err(()) => cursor.completed() as isize,
    }
}

/// 从 descriptor 读取至单一 userspace buffer。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `pointer`: userspace 输出地址。
/// - `length`: 最大读取长度。
///
/// # Returns
///
/// byte count、EOF 零或负 errno/internal restart sentinel。
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

/// 按 Linux LP64 `struct iovec` 顺序从同一个 OFD scatter read。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `iovector`: userspace `iovec` 数组地址；count 为零时可为空。
/// - `count`: iovec 数量，最大 1024。
///
/// # Returns
///
/// 总读取字节数；导入失败或首个 read 失败返回负 errno，已有进度后返回 partial count。
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

/// 将单一 userspace buffer 写入 descriptor。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `pointer`: userspace 输入地址。
/// - `length`: 待写入长度。
///
/// # Returns
///
/// byte count、partial count 或负 errno/internal restart sentinel。
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

/// 按 Linux LP64 `struct iovec` 顺序写入同一个 open file description。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `iovector`: userspace `iovec` 数组地址；count 为零时可为空。
/// - `count`: iovec 数量，最大 1024。
///
/// # Returns
///
/// 总写入字节数；导入失败或首个 write 失败返回负 errno，已有进度后返回 partial count。
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
