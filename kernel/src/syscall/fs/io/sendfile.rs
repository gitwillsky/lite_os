use super::*;

const MAX_RW_COUNT: usize = 0x7fff_f000;

mod regular_transfer;
use regular_transfer::copy_regular_file;

/// 按全局 OFD identity 顺序取得两个共享 offset，避免反向 sendfile 形成 ABBA。
///
/// # Parameters
///
/// - `task`: 当前 caller。
/// - `input`: 输入 OFD 与 page-cache facade。
/// - `output`: 输出 OFD 与 page-cache facade。
/// - `count`: 最大传输长度。
///
/// # Returns
///
/// copy byte count、EOF、partial count 或负 errno。
fn copy_from_shared_offset(
    task: &TaskControlBlock,
    input_ofd: &Arc<OpenFileDescription>,
    output_ofd: &Arc<OpenFileDescription>,
    input: &RegularFile,
    output: &RegularFile,
    count: usize,
) -> isize {
    if Arc::ptr_eq(input_ofd, output_ofd) {
        let position = match input_ofd.position_snapshot() {
            Ok(position) => position,
            Err(error) => return crate::syscall::fs::position_error(error),
        };
        return if count == 0 || position >= input.size() {
            0
        } else {
            -errno::EINVAL
        };
    }
    OpenFileDescription::with_positions(input_ofd, output_ofd, |input_position, output_position| {
        copy_regular_file(task, input, output, input_position, output_position, count)
    })
    .unwrap_or_else(crate::syscall::fs::position_error)
}

/// 完成 descriptor 校验并执行 regular-file 到 regular-file copy。
///
/// # Parameters
///
/// - `task`: 当前 caller 与 fd-table owner。
/// - `output_fd`: 以 write access 打开的输出 descriptor。
/// - `input_fd`: 以 read access 打开的输入 descriptor。
/// - `input_position`: 显式输入 offset；为空时使用并更新输入 OFD offset。
/// - `count`: 最大传输长度；按 Linux MAX_RW_COUNT 截断。
///
/// # Returns
///
/// 已传输字节数、EOF 零、partial count 或负 errno。
///
/// # Errors
///
/// - descriptor/access 错误返回 `EBADF`；当前 scope 外 backend 返回 `EINVAL/ESPIPE`。
/// - 重叠同文件区间返回 `EINVAL`。
/// - 输出带 O_APPEND 返回 `EINVAL`；storage、内存与 RLIMIT 错误透传对应 errno。
fn do_sendfile(
    task: &TaskControlBlock,
    output_fd: usize,
    input_fd: usize,
    input_position: Option<&mut u64>,
    count: usize,
) -> isize {
    let Some((output_ofd, input_ofd)) =
        task.with_file_descriptions(output_fd, input_fd, |output, input| (output, input))
    else {
        return -errno::EBADF;
    };
    if input_ofd.status_flags() & O_ACCMODE == O_WRONLY
        || output_ofd.status_flags() & O_ACCMODE == O_RDONLY
    {
        return -errno::EBADF;
    }
    if output_ofd.status_flags() & O_APPEND != 0 {
        return -errno::EINVAL;
    }
    let OpenFileKind::Inode = &input_ofd.kind else {
        return if input_position.is_none() {
            -errno::EINVAL
        } else {
            -errno::ESPIPE
        };
    };
    let OpenFileKind::Inode = &output_ofd.kind else {
        return -errno::EINVAL;
    };
    let input_inode = input_ofd.inode_ref();
    let output_inode = output_ofd.inode_ref();
    if input_inode.inode_type() != InodeType::File || output_inode.inode_type() != InodeType::File {
        return -errno::EINVAL;
    }
    let input = match RegularFile::from_inode(input_inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let output = match RegularFile::from_inode(output_inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let count = count.min(MAX_RW_COUNT);

    let result = if let Some(input_position) = input_position {
        output_ofd
            .with_position(|output_position| {
                copy_regular_file(
                    task,
                    &input,
                    &output,
                    input_position,
                    output_position,
                    count,
                )
            })
            .unwrap_or_else(crate::syscall::fs::position_error)
    } else {
        copy_from_shared_offset(task, &input_ofd, &output_ofd, &input, &output, count)
    };
    if result > 0 {
        input_ofd.touch_access();
        input_ofd.notify(crate::fs::IN_ACCESS);
        output_ofd.notify(crate::fs::IN_MODIFY);
    }
    result
}

/// 实现 Linux 64-bit `sendfile` 的 regular-file 到 regular-file 数据路径。
///
/// # Parameters
///
/// - `output_fd`: 以 write access 打开的输出 descriptor。
/// - `input_fd`: 以 read access 打开的输入 descriptor。
/// - `offset`: 可空的 userspace signed 64-bit 输入 offset；非空时不修改输入 OFD offset。
/// - `count`: 最大传输长度；按 Linux MAX_RW_COUNT 截断。
///
/// # Returns
///
/// 已传输字节数、EOF 零、partial count 或负 errno。
///
/// # Errors
///
/// - 坏 offset pointer 返回 `EFAULT`；非法 signed offset 返回 `EINVAL`。
/// - descriptor、backend、storage、重叠区间与 RLIMIT 错误由数据路径返回。
pub(crate) fn sys_sendfile(
    output_fd: usize,
    input_fd: usize,
    offset: usize,
    count: usize,
) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    if offset == 0 {
        let result = do_sendfile(&task, output_fd, input_fd, None, count);
        task.account_read_result(result);
        task.account_write_result(result);
        return result;
    }
    let mut bytes = [0u8; core::mem::size_of::<i64>()];
    if task.copy_from_user(offset, &mut bytes).is_err() {
        return -errno::EFAULT;
    }
    let mut signed_position = i64::from_ne_bytes(bytes);
    let result = match u64::try_from(signed_position) {
        Ok(mut input_position) => {
            let result = do_sendfile(&task, output_fd, input_fd, Some(&mut input_position), count);
            signed_position = match i64::try_from(input_position) {
                Ok(position) => position,
                Err(_) => return -errno::EOVERFLOW,
            };
            result
        }
        Err(_) => -errno::EINVAL,
    };
    task.account_read_result(result);
    task.account_write_result(result);
    if task
        .copy_to_user(offset, &signed_position.to_ne_bytes())
        .is_err()
    {
        return -errno::EFAULT;
    }
    result
}

/// 完成 `copy_file_range` 的 descriptor 校验并执行 regular-file 到 regular-file copy。
///
/// 与 `sendfile` 共用同一条数据路径；两侧 offset 各自独立地选择显式值或 OFD 共享 offset。
///
/// # Parameters
///
/// - `task`: 当前 caller 与 fd-table owner。
/// - `input_fd` / `output_fd`: 以 read / write access 打开的 descriptor。
/// - `input_position` / `output_position`: 显式 offset；为空时使用并更新对应 OFD offset。
/// - `count`: 最大传输长度；按 Linux MAX_RW_COUNT 截断。
///
/// # Returns
///
/// 已传输字节数、EOF 零、partial count 或负 errno。
///
/// # Errors
///
/// - descriptor 缺失、access 不符或输出带 O_APPEND 返回 `EBADF`。
/// - 目录返回 `EISDIR`，其他非 regular file 返回 `EINVAL`。
/// - 同一文件的传输区间重叠返回 `EINVAL`；storage、内存与 RLIMIT 错误透传对应 errno。
fn do_copy_file_range(
    task: &TaskControlBlock,
    input_fd: usize,
    input_position: Option<&mut u64>,
    output_fd: usize,
    output_position: Option<&mut u64>,
    count: usize,
) -> isize {
    let Some((input_ofd, output_ofd)) =
        task.with_file_descriptions(input_fd, output_fd, |input, output| (input, output))
    else {
        return -errno::EBADF;
    };
    if input_ofd.status_flags() & O_ACCMODE == O_WRONLY
        || output_ofd.status_flags() & O_ACCMODE == O_RDONLY
        || output_ofd.status_flags() & O_APPEND != 0
    {
        return -errno::EBADF;
    }
    let (OpenFileKind::Inode, OpenFileKind::Inode) = (&input_ofd.kind, &output_ofd.kind) else {
        return -errno::EINVAL;
    };
    let input_inode = input_ofd.inode_ref();
    let output_inode = output_ofd.inode_ref();
    for inode_type in [input_inode.inode_type(), output_inode.inode_type()] {
        match inode_type {
            InodeType::File => {}
            InodeType::Directory => return -errno::EISDIR,
            _ => return -errno::EINVAL,
        }
    }
    let input = match RegularFile::from_inode(input_inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let output = match RegularFile::from_inode(output_inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let count = count.min(MAX_RW_COUNT);
    let result = match (input_position, output_position) {
        (Some(input_position), Some(output_position)) => copy_regular_file(
            task,
            &input,
            &output,
            input_position,
            output_position,
            count,
        ),
        (Some(input_position), None) => output_ofd
            .with_position(|output_position| {
                copy_regular_file(
                    task,
                    &input,
                    &output,
                    input_position,
                    output_position,
                    count,
                )
            })
            .unwrap_or_else(crate::syscall::fs::position_error),
        (None, Some(output_position)) => input_ofd
            .with_position(|input_position| {
                copy_regular_file(
                    task,
                    &input,
                    &output,
                    input_position,
                    output_position,
                    count,
                )
            })
            .unwrap_or_else(crate::syscall::fs::position_error),
        (None, None) => {
            copy_from_shared_offset(task, &input_ofd, &output_ofd, &input, &output, count)
        }
    };
    if result > 0 {
        input_ofd.touch_access();
        input_ofd.notify(crate::fs::IN_ACCESS);
        output_ofd.notify(crate::fs::IN_MODIFY);
    }
    result
}

/// 读取可空的 `loff_t *` 偏移；负值非法。
fn read_file_offset(task: &TaskControlBlock, pointer: usize) -> Result<Option<u64>, isize> {
    if pointer == 0 {
        return Ok(None);
    }
    let mut bytes = [0u8; core::mem::size_of::<i64>()];
    task.copy_from_user(pointer, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    u64::try_from(i64::from_ne_bytes(bytes))
        .map(Some)
        .map_err(|_| -errno::EINVAL)
}

/// 实现 Linux 64-bit `copy_file_range` 的 regular-file 到 regular-file 数据路径。
///
/// # Parameters
///
/// - `input_fd` / `input_offset`: 输入 descriptor 与可空 `loff_t *`；非空时不修改输入 OFD offset。
/// - `output_fd` / `output_offset`: 输出 descriptor 与可空 `loff_t *`；语义同上。
/// - `count`: 最大传输长度；按 Linux MAX_RW_COUNT 截断。
/// - `flags`: 必须为零。
///
/// # Returns
///
/// 已传输字节数、EOF 零、partial count 或负 errno；只有传输了数据时才写回显式 offset。
///
/// # Errors
///
/// - 非零 flags、负 offset 或 offset 加长度溢出返回 `EINVAL`/`EOVERFLOW`；坏指针返回 `EFAULT`。
/// - descriptor 与数据路径错误见 [`do_copy_file_range`]。
pub(crate) fn sys_copy_file_range(
    input_fd: usize,
    input_offset: usize,
    output_fd: usize,
    output_offset: usize,
    count: usize,
    flags: usize,
) -> isize {
    if flags != 0 {
        return -errno::EINVAL;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    // Linux 先复制两个 offset（EFAULT 优先），再取 descriptor。
    let mut input_position = match read_file_offset(&task, input_offset) {
        Ok(position) => position,
        Err(error) => return error,
    };
    let mut output_position = match read_file_offset(&task, output_offset) {
        Ok(position) => position,
        Err(error) => return error,
    };
    let bounded = count.min(MAX_RW_COUNT) as u64;
    for position in [input_position, output_position].into_iter().flatten() {
        if position
            .checked_add(bounded)
            .is_none_or(|end| end > i64::MAX as u64)
        {
            return -errno::EOVERFLOW;
        }
    }
    let result = do_copy_file_range(
        &task,
        input_fd,
        input_position.as_mut(),
        output_fd,
        output_position.as_mut(),
        count,
    );
    task.account_read_result(result);
    task.account_write_result(result);
    if result > 0 {
        for (pointer, position) in [
            (input_offset, input_position),
            (output_offset, output_position),
        ] {
            if let Some(position) = position
                && task
                    .copy_to_user(pointer, &(position as i64).to_ne_bytes())
                    .is_err()
            {
                return -errno::EFAULT;
            }
        }
    }
    result
}
