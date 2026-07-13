use super::*;
mod write_limit;
use write_limit::{bounded_regular_write, file_size_exceeded};
mod vector;
pub(crate) use vector::sys_readv;
use vector::{IOV_MAX, UserIoVec};

/// @description 把 task-layer pipe wait result 统一翻译为 syscall control flow。
///
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

/// @description 从 descriptor 读取至 userspace buffer。
///
/// @param fd 源 descriptor。
/// @param pointer userspace 输出地址。
/// @param length 最大读取长度。
/// @return byte count、EOF 零或负 errno/internal restart sentinel。
pub(crate) fn sys_read(fd: usize, pointer: *mut u8, length: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_WRONLY {
        return -errno::EBADF;
    }
    if let OpenFileKind::Character(device) = &ofd.kind {
        match device {
            CharacterDevice::Null => return 0,
            CharacterDevice::Zero => {
                let zeroes = [0u8; 512];
                let mut copied = 0;
                while copied < length {
                    let count = (length - copied).min(zeroes.len());
                    if task
                        .copy_to_user(pointer as usize + copied, &zeroes[..count])
                        .is_err()
                    {
                        return if copied == 0 {
                            -errno::EFAULT
                        } else {
                            copied as isize
                        };
                    }
                    copied += count;
                }
                return copied as isize;
            }
            CharacterDevice::Terminal {
                terminal: console,
                kind,
            } => {
                if length == 0 {
                    return 0;
                }
                let mut chunk = [0u8; 512];
                loop {
                    if *kind == DeviceKind::Tty
                        && let Err(error) = guard_terminal_access(console, TerminalAccess::Input)
                    {
                        return error;
                    }
                    if drain_terminal_input(console).is_err() {
                        return -errno::EIO;
                    }
                    let count = length.min(chunk.len());
                    let read = match console.read(&mut chunk[..count]) {
                        TerminalRead::Empty => {
                            match crate::task::wait_for_console(|| console.wait_ready()) {
                                crate::task::WaitResult::Woken => continue,
                                crate::task::WaitResult::Interrupted => return -errno::EINTR,
                                crate::task::WaitResult::TimedOut => {
                                    panic!("console wait cannot time out")
                                }
                            }
                        }
                        TerminalRead::Bytes(read) => read,
                        TerminalRead::Eof => return 0,
                    };
                    return task
                        .copy_to_user(pointer as usize, &chunk[..read])
                        .map_or(-errno::EFAULT, |()| read as isize);
                }
            }
        }
    }
    if let OpenFileKind::Pipe(endpoint) = &ofd.kind {
        if endpoint.direction() != PipeDirection::Read {
            return -errno::EBADF;
        }
        if length == 0 {
            return 0;
        }
        let mut chunk = [0u8; 512];
        loop {
            let count = length.min(chunk.len());
            match endpoint.read(&mut chunk[..count]) {
                PipeRead::Bytes(read) => {
                    return task
                        .copy_to_user(pointer as usize, &chunk[..read])
                        .map_or(-errno::EFAULT, |()| read as isize);
                }
                PipeRead::Eof => return 0,
                PipeRead::Empty if *ofd.flags.lock() & O_NONBLOCK != 0 => return -errno::EAGAIN,
                PipeRead::Empty => {
                    if let Err(error) = block_on_pipe(&endpoint.pipe(), PipeWaitCondition::Readable)
                    {
                        return error;
                    }
                }
            }
        }
    }
    if let OpenFileKind::Socket(socket) = &ofd.kind {
        if length == 0 {
            return 0;
        }
        let mut chunk = [0u8; 512];
        let count = length.min(chunk.len());
        loop {
            match socket.read(&mut chunk[..count]) {
                Ok(read) => {
                    return task
                        .copy_to_user(pointer as usize, &chunk[..read])
                        .map_or(-errno::EFAULT, |()| read as isize);
                }
                Err(crate::socket::SocketError::Again) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                    return -errno::EAGAIN;
                }
                Err(crate::socket::SocketError::Again) => {
                    match crate::syscall::poll::wait_for_ofd(&ofd, 1) {
                        WaitResult::Woken => {}
                        WaitResult::Interrupted => return -errno::EINTR,
                        WaitResult::TimedOut => unreachable!(),
                    }
                }
                Err(error) => return crate::syscall::socket::socket_error(error),
            }
        }
    }
    if matches!(&ofd.kind, OpenFileKind::Epoll(_)) {
        return -errno::EINVAL;
    }
    let OpenFileKind::Inode(opened) = &ofd.kind else {
        unreachable!("character device handled above")
    };
    let inode = opened.inode();
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    let mut offset = ofd.offset.lock();
    let mut total = 0;
    let mut chunk = [0u8; 512];
    while total < length {
        let count = chunk.len().min(length - total);
        let got = match crate::fs::read(inode.clone(), *offset, &mut chunk[..count]) {
            Ok(v) => v,
            Err(e) => return if total == 0 { ferr(e) } else { total as isize },
        };
        if got == 0 {
            break;
        }
        if task
            .copy_to_user(pointer as usize + total, &chunk[..got])
            .is_err()
        {
            return if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        }
        *offset += got as u64;
        total += got;
    }
    total as isize
}

/// @description 将 userspace buffer 写入 descriptor。
///
/// @param fd 目标 descriptor。
/// @param pointer userspace 输入地址。
/// @param length 待写入长度。
/// @return byte count、partial count 或负 errno/internal restart sentinel。
pub(crate) fn sys_write(fd: usize, pointer: *const u8, length: usize) -> isize {
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    if *ofd.flags.lock() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    if let OpenFileKind::Pipe(endpoint) = &ofd.kind {
        if endpoint.direction() != PipeDirection::Write {
            return -errno::EBADF;
        }
        if length == 0 {
            return 0;
        }
        let mut chunk = [0u8; PIPE_BUF];
        let mut total = 0;
        while total < length {
            let count = (length - total).min(chunk.len());
            if task
                .copy_from_user(pointer as usize + total, &mut chunk[..count])
                .is_err()
            {
                return if total == 0 {
                    -errno::EFAULT
                } else {
                    total as isize
                };
            }
            match endpoint.write(&chunk[..count]) {
                PipeWrite::Bytes(written) => {
                    total += written;
                    if written < count {
                        return total as isize;
                    }
                }
                PipeWrite::Full if total != 0 => return total as isize,
                PipeWrite::Full if *ofd.flags.lock() & O_NONBLOCK != 0 => return -errno::EAGAIN,
                PipeWrite::Full => {
                    if let Err(error) = block_on_pipe(
                        &endpoint.pipe(),
                        PipeWaitCondition::Writable { minimum: count },
                    ) {
                        return error;
                    }
                }
                PipeWrite::Broken => {
                    send_thread_signal(task.tgid(), task.tid(), 13)
                        .expect("current pipe writer must exist");
                    return if total == 0 {
                        -errno::EPIPE
                    } else {
                        total as isize
                    };
                }
            }
        }
        return total as isize;
    }
    if let OpenFileKind::Socket(socket) = &ofd.kind {
        if length == 0 {
            return 0;
        }
        let mut chunk = [0u8; PIPE_BUF];
        let count = length.min(chunk.len());
        if task
            .copy_from_user(pointer as usize, &mut chunk[..count])
            .is_err()
        {
            return -errno::EFAULT;
        }
        loop {
            match socket.write(&chunk[..count]) {
                Ok(written) => return written as isize,
                Err(crate::socket::SocketError::Again) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                    return -errno::EAGAIN;
                }
                Err(crate::socket::SocketError::Again) => {
                    match crate::syscall::poll::wait_for_ofd(&ofd, 4) {
                        WaitResult::Woken => {}
                        WaitResult::Interrupted => return -errno::EINTR,
                        WaitResult::TimedOut => unreachable!(),
                    }
                }
                Err(crate::socket::SocketError::BrokenPipe) => {
                    send_thread_signal(task.tgid(), task.tid(), 13)
                        .expect("socket writer must exist");
                    return -errno::EPIPE;
                }
                Err(error) => return crate::syscall::socket::socket_error(error),
            }
        }
    }
    if matches!(&ofd.kind, OpenFileKind::Epoll(_)) {
        return -errno::EINVAL;
    }
    if let OpenFileKind::Character(CharacterDevice::Terminal {
        terminal,
        kind: DeviceKind::Tty,
    }) = &ofd.kind
        && let Err(error) = guard_terminal_access(terminal, TerminalAccess::Output)
    {
        return error;
    }
    let mut offset = ofd.offset.lock();
    let mut total = 0;
    let mut chunk = [0u8; 512];
    while total < length {
        let count = match bounded_regular_write(
            &task,
            &ofd,
            *offset,
            chunk.len().min(length - total),
            total,
        ) {
            Ok(count) => count,
            Err(result) => return result,
        };
        if task
            .copy_from_user(pointer as usize + total, &mut chunk[..count])
            .is_err()
        {
            return if total == 0 {
                -errno::EFAULT
            } else {
                total as isize
            };
        }
        let wrote = match &ofd.kind {
            OpenFileKind::Pipe(_) => unreachable!("pipe handled before offset-backed write"),
            OpenFileKind::Socket(_) | OpenFileKind::Epoll(_) => {
                unreachable!("anonymous descriptor handled above")
            }
            OpenFileKind::Character(device) => match device {
                CharacterDevice::Null | CharacterDevice::Zero => count,
                CharacterDevice::Terminal {
                    terminal: console, ..
                } => match console.write(&chunk[..count]) {
                    Ok(written) => written,
                    Err(error) => {
                        return if total == 0 {
                            ferr(error)
                        } else {
                            total as isize
                        };
                    }
                },
            },
            OpenFileKind::Inode(opened) => {
                let inode = opened.inode();
                if *ofd.flags.lock() & O_APPEND != 0 {
                    match crate::fs::append(inode.clone(), &chunk[..count], task.file_size_limit())
                    {
                        Ok((append_offset, written)) => {
                            if written == 0 && count != 0 {
                                return if total == 0 {
                                    file_size_exceeded(&task)
                                } else {
                                    total as isize
                                };
                            }
                            *offset = append_offset + written as u64;
                            total += written;
                            if written < count {
                                break;
                            }
                            continue;
                        }
                        Err(error) => {
                            return if total == 0 {
                                ferr(error)
                            } else {
                                total as isize
                            };
                        }
                    }
                }
                match crate::fs::write(inode.clone(), *offset, &chunk[..count]) {
                    Ok(v) => v,
                    Err(e) => return if total == 0 { ferr(e) } else { total as isize },
                }
            }
        };
        *offset += wrote as u64;
        total += wrote;
        if wrote < count {
            break;
        }
    }
    total as isize
}

/// @description 按 Linux RV64 `struct iovec` 顺序写入同一个 open file description。
///
/// @param fd 目标 descriptor。
/// @param iovector userspace `iovec` 数组地址；count 为零时可为空。
/// @param count iovec 数量，最大 1024。
/// @return 总写入字节数；导入失败或首个 write 失败返回负 errno，已有进度后返回 partial count。
pub(crate) fn sys_writev(fd: usize, iovector: usize, count: usize) -> isize {
    if count > IOV_MAX {
        return -errno::EINVAL;
    }
    if count == 0 {
        return sys_write(fd, core::ptr::null(), 0);
    }
    if iovector == 0 {
        return -errno::EFAULT;
    }
    let Some(task) = current_task() else {
        return -errno::ESRCH;
    };
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    let mut vectors = Vec::new();
    if vectors.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    let mut total_length = 0usize;
    for index in 0..count {
        let offset = match index.checked_mul(mem::size_of::<UserIoVec>()) {
            Some(offset) => offset,
            None => return -errno::EFAULT,
        };
        let address = match iovector.checked_add(offset) {
            Some(address) => address,
            None => return -errno::EFAULT,
        };
        let mut bytes = [0u8; mem::size_of::<UserIoVec>()];
        if task.copy_from_user(address, &mut bytes).is_err() {
            return -errno::EFAULT;
        }
        let vector = UserIoVec {
            base: usize::from_ne_bytes(bytes[..mem::size_of::<usize>()].try_into().unwrap()),
            length: usize::from_ne_bytes(bytes[mem::size_of::<usize>()..].try_into().unwrap()),
        };
        total_length = match total_length.checked_add(vector.length) {
            Some(length) if length <= isize::MAX as usize => length,
            _ => return -errno::EINVAL,
        };
        vectors.push(vector);
    }
    if total_length == 0 {
        return 0;
    }
    if total_length <= PIPE_BUF
        && let OpenFileKind::Pipe(endpoint) = &ofd.kind
    {
        if endpoint.direction() != PipeDirection::Write {
            return -errno::EBADF;
        }
        let mut input = [0u8; PIPE_BUF];
        let mut copied = 0;
        for vector in &vectors {
            if task
                .copy_from_user(vector.base, &mut input[copied..copied + vector.length])
                .is_err()
            {
                return -errno::EFAULT;
            }
            copied += vector.length;
        }
        loop {
            match endpoint.write(&input[..total_length]) {
                PipeWrite::Bytes(written) => {
                    assert_eq!(written, total_length, "PIPE_BUF writev lost atomicity");
                    return written as isize;
                }
                PipeWrite::Full if *ofd.flags.lock() & O_NONBLOCK != 0 => return -errno::EAGAIN,
                PipeWrite::Full => {
                    if let Err(error) = block_on_pipe(
                        &endpoint.pipe(),
                        PipeWaitCondition::Writable {
                            minimum: total_length,
                        },
                    ) {
                        return error;
                    }
                }
                PipeWrite::Broken => {
                    send_thread_signal(task.tgid(), task.tid(), 13)
                        .expect("current pipe writev task must exist");
                    return -errno::EPIPE;
                }
            }
        }
    }

    let mut written = 0usize;
    for vector in vectors {
        if vector.length == 0 {
            continue;
        }
        let result = sys_write(fd, vector.base as *const u8, vector.length);
        if result < 0 {
            return if written == 0 {
                result
            } else {
                written as isize
            };
        }
        let result = result as usize;
        written += result;
        if result < vector.length {
            break;
        }
    }
    written as isize
}
