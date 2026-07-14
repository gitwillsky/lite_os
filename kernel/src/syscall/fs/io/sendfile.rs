use super::*;

const MAX_RW_COUNT: usize = 0x7fff_f000;

/// @description 将一次 regular-file 到 regular-file 的 kernel-owned copy 提交给 page cache。
/// @param task 当前 caller，提供 RLIMIT_FSIZE 与 SIGXFSZ target。
/// @param input 已解析的输入 page-cache facade。
/// @param output 已解析的输出 page-cache facade。
/// @param input_position 本次操作唯一输入 offset owner。
/// @param output_position 本次操作唯一输出 OFD offset owner。
/// @param count Linux MAX_RW_COUNT 截断后的最大传输长度。
/// @return 已传输字节数、EOF 零、首错负 errno 或已有进度后的 partial count。
/// @error 同一文件的实际传输区间重叠返回 `EINVAL`。
/// @error 输出越过 RLIMIT_FSIZE 时返回 `EFBIG` 并投递 SIGXFSZ。
fn copy_regular_file(
    task: &TaskControlBlock,
    input: &RegularFile,
    output: &RegularFile,
    input_position: &mut u64,
    output_position: &mut u64,
    count: usize,
) -> isize {
    let available = usize::try_from(input.size().saturating_sub(*input_position))
        .unwrap_or(usize::MAX)
        .min(count);
    if available == 0 {
        return 0;
    }
    let transferable = match bounded_regular_write(task, *output_position, available, 0) {
        Ok(count) => count,
        Err(error) => return error,
    };
    if input.id() == output.id() {
        let input_end = u128::from(*input_position) + transferable as u128;
        let output_end = u128::from(*output_position) + transferable as u128;
        if output_end > u128::from(*input_position) && u128::from(*output_position) < input_end {
            return -errno::EINVAL;
        }
    }

    let writer = output.begin_write();
    let mut chunk = [0u8; crate::memory::PAGE_SIZE];
    let mut total = 0usize;
    while total < transferable {
        let requested = chunk.len().min(transferable - total);
        let read = match input.read(*input_position, &mut chunk[..requested]) {
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
        let written = match writer.write(*output_position, &chunk[..read]) {
            Ok(written) => written,
            Err(error) => {
                return if total == 0 {
                    ferr(error)
                } else {
                    total as isize
                };
            }
        };
        *input_position = input_position
            .checked_add(written as u64)
            .expect("sendfile input position overflow");
        *output_position = output_position
            .checked_add(written as u64)
            .expect("sendfile output position overflow");
        total += written;
        if written < read {
            break;
        }
    }
    total as isize
}

/// @description 按全局 OFD identity 顺序取得两个共享 offset，避免反向 sendfile 形成 ABBA。
/// @param task 当前 caller。
/// @param input 输入 OFD 与 page-cache facade。
/// @param output 输出 OFD 与 page-cache facade。
/// @param count 最大传输长度。
/// @return copy byte count、EOF、partial count 或负 errno。
fn copy_from_shared_offset(
    task: &TaskControlBlock,
    input_ofd: &Arc<OpenFileDescription>,
    output_ofd: &Arc<OpenFileDescription>,
    input: &RegularFile,
    output: &RegularFile,
    count: usize,
) -> isize {
    if Arc::ptr_eq(input_ofd, output_ofd) {
        let position = *input_ofd.offset.lock();
        return if count == 0 || position >= input.size() {
            0
        } else {
            -errno::EINVAL
        };
    }
    if Arc::as_ptr(input_ofd) < Arc::as_ptr(output_ofd) {
        let mut input_position = input_ofd.offset.lock();
        let mut output_position = output_ofd.offset.lock();
        copy_regular_file(
            task,
            input,
            output,
            &mut input_position,
            &mut output_position,
            count,
        )
    } else {
        let mut output_position = output_ofd.offset.lock();
        let mut input_position = input_ofd.offset.lock();
        copy_regular_file(
            task,
            input,
            output,
            &mut input_position,
            &mut output_position,
            count,
        )
    }
}

/// @description 完成 descriptor 校验并执行 regular-file 到 regular-file copy。
/// @param task 当前 caller 与 fd-table owner。
/// @param output_fd 以 write access 打开的输出 descriptor。
/// @param input_fd 以 read access 打开的输入 descriptor。
/// @param input_position 显式输入 offset；为空时使用并更新输入 OFD offset。
/// @param count 最大传输长度；按 Linux MAX_RW_COUNT 截断。
/// @return 已传输字节数、EOF 零、partial count 或负 errno。
/// @error descriptor/access 错误返回 `EBADF`；当前 scope 外 backend 返回 `EINVAL/ESPIPE`。
/// @error 重叠同文件区间返回 `EINVAL`。
/// @error 输出带 O_APPEND 返回 `EINVAL`；storage、内存与 RLIMIT 错误透传对应 errno。
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
    if *input_ofd.flags.lock() & O_ACCMODE == O_WRONLY
        || *output_ofd.flags.lock() & O_ACCMODE == O_RDONLY
    {
        return -errno::EBADF;
    }
    if *output_ofd.flags.lock() & O_APPEND != 0 {
        return -errno::EINVAL;
    }
    let OpenFileKind::Inode(input_opened) = &input_ofd.kind else {
        return if input_position.is_none() {
            -errno::EINVAL
        } else {
            -errno::ESPIPE
        };
    };
    let OpenFileKind::Inode(output_opened) = &output_ofd.kind else {
        return -errno::EINVAL;
    };
    let input_inode = input_opened.inode();
    let output_inode = output_opened.inode();
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

    let Some(input_position) = input_position else {
        return copy_from_shared_offset(task, &input_ofd, &output_ofd, &input, &output, count);
    };
    let mut output_position = output_ofd.offset.lock();
    copy_regular_file(
        task,
        &input,
        &output,
        input_position,
        &mut output_position,
        count,
    )
}

/// @description 实现 Linux/riscv64 `sendfile` 的 regular-file 到 regular-file 数据路径。
/// @param output_fd 以 write access 打开的输出 descriptor。
/// @param input_fd 以 read access 打开的输入 descriptor。
/// @param offset 可空的 userspace signed 64-bit 输入 offset；非空时不修改输入 OFD offset。
/// @param count 最大传输长度；按 Linux MAX_RW_COUNT 截断。
/// @return 已传输字节数、EOF 零、partial count 或负 errno。
/// @error 坏 offset pointer 返回 `EFAULT`；非法 signed offset 返回 `EINVAL`。
/// @error descriptor、backend、storage、重叠区间与 RLIMIT 错误由数据路径返回。
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
        return do_sendfile(&task, output_fd, input_fd, None, count);
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
    if task
        .copy_to_user(offset, &signed_position.to_ne_bytes())
        .is_err()
    {
        return -errno::EFAULT;
    }
    result
}
