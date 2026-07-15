use super::*;
use crate::fs::TerminalReadMode;

/// @description 执行 scalar/readv 共用的唯一 sequential read descriptor dispatch。
/// @param task userspace address owner。
/// @param ofd 已完成 access/capability 检查的共享 OFD。
/// @param vectors scalar one-element 或已导入的 RV64 iovec 序列。
/// @param total_length vectors 的 checked 总 capacity。
/// @return byte count、EOF、partial count 或负 errno。
pub(super) fn read_descriptor(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    vectors: &[UserIoVec],
    total_length: usize,
) -> isize {
    if total_length == 0 {
        return 0;
    }
    match &ofd.kind {
        OpenFileKind::Inode(opened) => {
            let inode = opened.inode();
            if inode.inode_type() == InodeType::Directory {
                return -errno::EISDIR;
            }
            let file = match RegularFile::from_inode(inode) {
                Ok(file) => file,
                Err(error) => return ferr(error),
            };
            // 单个 sequential read 唯一持有 OFD offset；缺失该 ownership 会让共享 OFD
            // 的并发 reader 在 chunks 之间穿插，使一次 operation 返回不连续的文件区间。
            let mut offset = ofd.offset.lock();
            read_regular_vectors(task, &file, &mut offset, vectors)
        }
        OpenFileKind::Pipe(endpoint) => {
            if endpoint.direction() != PipeDirection::Read {
                return -errno::EBADF;
            }
            let mut input = match buffer(total_length.min(64 * 1024)) {
                Ok(input) => input,
                Err(error) => return error,
            };
            let read = loop {
                match endpoint.read(&mut input) {
                    PipeRead::Bytes(read) => break read,
                    PipeRead::Eof => return 0,
                    PipeRead::Empty if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                        return -errno::EAGAIN;
                    }
                    PipeRead::Empty => {
                        if let Err(error) =
                            block_on_pipe(&endpoint.pipe(), PipeWaitCondition::Readable)
                        {
                            return error;
                        }
                    }
                }
            };
            let mut cursor = UserIoCursor::new(vectors);
            let result = cursor.copy_to_user(task, &input[..read]);
            scatter_result(&cursor, result)
        }
        OpenFileKind::Socket(socket) => {
            // 1. Socket facade 从唯一 protocol policy 投影 bounded useful capacity。
            let capacity = socket.receive_staging_capacity(total_length, 64 * 1024);
            let mut input = match buffer(capacity) {
                Ok(input) => input,
                Err(error) => return error,
            };
            // 2. 一个 sequential read 只消费一次 socket receive operation；逐 chunk 调用
            // backend 会让 datagram 丢失消息边界，并让 stream 的 blocking 语义分裂。
            let read = loop {
                match socket.read(&mut input) {
                    Ok(read) => break read,
                    Err(crate::socket::SocketError::Again)
                        if *ofd.flags.lock() & O_NONBLOCK != 0 =>
                    {
                        return -errno::EAGAIN;
                    }
                    Err(crate::socket::SocketError::Again) => {
                        match crate::syscall::poll::wait_for_ofd(ofd, 1) {
                            WaitResult::Woken => {}
                            WaitResult::Interrupted => return -errno::EINTR,
                            WaitResult::TimedOut => unreachable!(),
                            WaitResult::OutOfMemory => return -errno::ENOMEM,
                        }
                    }
                    Err(error) => return crate::syscall::socket::socket_error(error),
                }
            };
            // 3. backend result 只由 cursor scatter 一次，partial copyout 不复制 progress state。
            let mut cursor = UserIoCursor::new(vectors);
            let result = cursor.copy_to_user(task, &input[..read]);
            scatter_result(&cursor, result)
        }
        OpenFileKind::EventFd(event) => {
            let size = mem::size_of::<u64>();
            // 1. Linux eventfd_read 只拒绝小于 u64 的 iterator；read(2) 同样以单元素
            // iov_iter 进入，因此大 buffer 必须成功，否则 libuv 会在 eventfd drain 时中止。
            if total_length < size {
                return -errno::EINVAL;
            }
            let mut cursor = UserIoCursor::new(vectors);
            if cursor.validate_write_prefix(task, size).is_err() {
                return -errno::EFAULT;
            }
            // 2. destructive counter read 只在 output prefix 已证明可写后执行。
            let value = loop {
                match event.read() {
                    crate::ipc::EventFdRead::Value(value) => break value,
                    crate::ipc::EventFdRead::Empty if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                        return -errno::EAGAIN;
                    }
                    crate::ipc::EventFdRead::Empty => {
                        match crate::syscall::poll::wait_for_ofd(ofd, 1) {
                            WaitResult::Woken => {}
                            WaitResult::Interrupted => return -errno::EINTR,
                            WaitResult::TimedOut => unreachable!(),
                            WaitResult::OutOfMemory => return -errno::ENOMEM,
                        }
                    }
                }
            };
            // 3. Linux eventfd read_iter 只 scatter 一个 u64，即使剩余 capacity 更大。
            if cursor.copy_to_user(task, &value.to_ne_bytes()).is_err() {
                return -errno::EFAULT;
            }
            size as isize
        }
        OpenFileKind::Epoll(_) => unreachable!("epoll read rejected before descriptor dispatch"),
        OpenFileKind::Character(device) => match device {
            CharacterDevice::Null => 0,
            CharacterDevice::Zero => {
                let zeroes = [0u8; 512];
                let mut cursor = UserIoCursor::new(vectors);
                while cursor.completed() < total_length {
                    let count = (total_length - cursor.completed()).min(zeroes.len());
                    if cursor.copy_to_user(task, &zeroes[..count]).is_err() {
                        return if cursor.completed() == 0 {
                            -errno::EFAULT
                        } else {
                            cursor.completed() as isize
                        };
                    }
                }
                total_length as isize
            }
            CharacterDevice::Entropy => {
                let mut bytes = [0u8; 256];
                let mut cursor = UserIoCursor::new(vectors);
                while cursor.completed() < total_length {
                    let count = (total_length - cursor.completed()).min(bytes.len());
                    if crate::random::fill(&mut bytes[..count]).is_err() {
                        return if cursor.completed() == 0 {
                            -errno::EIO
                        } else {
                            cursor.completed() as isize
                        };
                    }
                    if cursor.copy_to_user(task, &bytes[..count]).is_err() {
                        return if cursor.completed() == 0 {
                            -errno::EFAULT
                        } else {
                            cursor.completed() as isize
                        };
                    }
                }
                total_length as isize
            }
            CharacterDevice::Drm(file) => {
                const EVENT_SIZE: usize = crate::drm::DrmEvent::SIZE;
                let maximum = total_length / EVENT_SIZE;
                let nonblocking = *ofd.flags.lock() & O_NONBLOCK != 0;
                let mut cursor = UserIoCursor::new(vectors);
                let mut events = [crate::drm::DrmEvent::EMPTY; 16];
                let mut consumed = 0usize;
                loop {
                    let readable = file.readable_event_count();
                    if readable == 0 {
                        if consumed != 0 {
                            break;
                        }
                        if nonblocking {
                            return -errno::EAGAIN;
                        }
                        match crate::syscall::poll::wait_for_ofd(ofd, 1) {
                            WaitResult::Woken => continue,
                            WaitResult::Interrupted => return -errno::EINTR,
                            WaitResult::TimedOut => unreachable!(),
                            WaitResult::OutOfMemory => return -errno::ENOMEM,
                        }
                    }
                    // Linux drm_read 对无法容纳队首完整 event 的 buffer 返回零，绝不拆分 ABI。
                    if maximum == consumed {
                        break;
                    }
                    let requested = readable.min(maximum - consumed).min(events.len());
                    if cursor
                        .validate_write_prefix(task, requested * EVENT_SIZE)
                        .is_err()
                    {
                        return if cursor.completed() == 0 {
                            -errno::EFAULT
                        } else {
                            cursor.completed() as isize
                        };
                    }
                    let read = file.read_events(&mut events[..requested]);
                    if read == 0 {
                        continue;
                    }
                    for event in events.iter().take(read) {
                        if cursor.copy_to_user(task, &event.encode()).is_err() {
                            return if cursor.completed() == 0 {
                                -errno::EFAULT
                            } else {
                                cursor.completed() as isize
                            };
                        }
                    }
                    consumed += read;
                    if consumed == maximum || read < requested {
                        break;
                    }
                }
                cursor.completed() as isize
            }
            CharacterDevice::PtyMaster(master) => {
                let mut input = [0u8; 512];
                let mut cursor = UserIoCursor::new(vectors);
                while cursor.completed() < total_length {
                    let requested = (total_length - cursor.completed()).min(input.len());
                    let read = loop {
                        match master.read(&mut input[..requested]) {
                            crate::ipc::PipeRead::Bytes(count) => break count,
                            crate::ipc::PipeRead::Eof => {
                                return if cursor.completed() == 0 {
                                    -errno::EIO
                                } else {
                                    cursor.completed() as isize
                                };
                            }
                            crate::ipc::PipeRead::Empty if master.peer_hung_up() => {
                                return if cursor.completed() == 0 {
                                    -errno::EIO
                                } else {
                                    cursor.completed() as isize
                                };
                            }
                            crate::ipc::PipeRead::Empty if cursor.completed() != 0 => {
                                return cursor.completed() as isize;
                            }
                            crate::ipc::PipeRead::Empty if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                                return -errno::EAGAIN;
                            }
                            crate::ipc::PipeRead::Empty => {
                                let wait = match master.prepare_to_block() {
                                    None => WaitResult::Woken,
                                    Some(pipe) => crate::task::wait_for_pipe(
                                        &pipe,
                                        crate::ipc::PipeWaitCondition::Readable,
                                    ),
                                };
                                match wait {
                                    WaitResult::Woken => {}
                                    WaitResult::Interrupted => return -errno::EINTR,
                                    WaitResult::TimedOut => unreachable!(),
                                    WaitResult::OutOfMemory => return -errno::ENOMEM,
                                }
                            }
                        }
                    };
                    let result = cursor.copy_to_user(task, &input[..read]);
                    if result.is_err() {
                        return scatter_result(&cursor, result);
                    }
                    if read < requested {
                        break;
                    }
                }
                cursor.completed() as isize
            }
            CharacterDevice::Input { file, .. } => {
                const EVENT_SIZE: usize = 24;
                if total_length < EVENT_SIZE {
                    return -errno::EINVAL;
                }
                let maximum = total_length / EVENT_SIZE;
                let nonblocking = *ofd.flags.lock() & O_NONBLOCK != 0;
                let mut cursor = UserIoCursor::new(vectors);
                let mut events = [crate::input::InputEvent::default(); 16];
                let mut consumed = 0usize;
                loop {
                    let available = file.readable_count().min(maximum - consumed);
                    if available == 0 {
                        if consumed != 0 || consumed == maximum {
                            break;
                        }
                        if nonblocking {
                            return -errno::EAGAIN;
                        }
                        match crate::syscall::poll::wait_for_ofd(ofd, 1) {
                            WaitResult::Woken => continue,
                            WaitResult::Interrupted => return -errno::EINTR,
                            WaitResult::TimedOut => unreachable!(),
                            WaitResult::OutOfMemory => return -errno::ENOMEM,
                        }
                    }
                    let requested = available.min(events.len());
                    // 1. destructive queue progress 前先 fault-in 整数个 event 的输出范围；
                    // 缺失该证明会在 EFAULT 时静默丢失已从 evdev ring 弹出的输入事件。
                    if cursor
                        .validate_write_prefix(task, requested * EVENT_SIZE)
                        .is_err()
                    {
                        return if cursor.completed() == 0 {
                            -errno::EFAULT
                        } else {
                            cursor.completed() as isize
                        };
                    }
                    // 2. 并发 reader 可能先消费同一 OFD queue，因此只提交实际取得的 events。
                    let read = file.read(&mut events[..requested]);
                    if read == 0 {
                        if consumed != 0 {
                            break;
                        }
                        continue;
                    }
                    // 3. ABI 只发布完整的 24-byte RV64 input_event，不暴露内部 raw event shape。
                    for event in events.iter().take(read) {
                        if cursor.copy_to_user(task, &event.encode()).is_err() {
                            return if cursor.completed() == 0 {
                                -errno::EFAULT
                            } else {
                                cursor.completed() as isize
                            };
                        }
                    }
                    consumed += read;
                    if consumed == maximum || read < requested {
                        break;
                    }
                }
                cursor.completed() as isize
            }
            CharacterDevice::Terminal {
                terminal: console,
                kind,
                pty,
            } => {
                let mut input = [0u8; 512];
                let capacity = total_length.min(input.len());
                let mode = console.read_mode(capacity);
                let nonblocking = *ofd.flags.lock() & O_NONBLOCK != 0;
                let mut read = 0;
                // VTIME 在 MIN=0 时从 read 开始计时，在 MIN>0 时从首字节开始并按
                // 后续每批输入重置；缺少这个区分会让 curses halfdelay 永久阻塞。
                let mut deadline = match mode {
                    TerminalReadMode::Noncanonical {
                        minimum: 0,
                        timeout_ns,
                    } if timeout_ns != 0 => {
                        Some(crate::timer::get_time_ns().saturating_add(timeout_ns))
                    }
                    _ => None,
                };
                loop {
                    if matches!(*kind, DeviceKind::Tty | DeviceKind::PtySlave(_))
                        && let Err(error) = guard_terminal_access(console, TerminalAccess::Input)
                    {
                        return error;
                    }
                    if drain_terminal_input(console).is_err() {
                        return -errno::EIO;
                    }
                    match console.read(&mut input[read..capacity]) {
                        TerminalRead::Empty => {
                            if let Some(slave) = pty
                                && slave.master_hung_up()
                            {
                                break;
                            }
                            if matches!(
                                mode,
                                TerminalReadMode::Noncanonical {
                                    minimum: 0,
                                    timeout_ns: 0,
                                }
                            ) {
                                break;
                            }
                            if nonblocking {
                                if read == 0 {
                                    return -errno::EAGAIN;
                                }
                                break;
                            }
                            let wait = if let Some(slave) = pty {
                                match slave.prepare_to_block() {
                                    None => crate::task::WaitResult::Woken,
                                    Some(pipe) => crate::task::wait_for_pipe_until(
                                        &pipe,
                                        crate::ipc::PipeWaitCondition::Readable,
                                        deadline,
                                    ),
                                }
                            } else {
                                crate::task::wait_for_console(deadline, || console.wait_ready())
                            };
                            match wait {
                                crate::task::WaitResult::Woken => continue,
                                crate::task::WaitResult::Interrupted if read == 0 => {
                                    return -errno::EINTR;
                                }
                                crate::task::WaitResult::Interrupted
                                | crate::task::WaitResult::TimedOut => break,
                                crate::task::WaitResult::OutOfMemory if read == 0 => {
                                    return -errno::ENOMEM;
                                }
                                crate::task::WaitResult::OutOfMemory => break,
                            }
                        }
                        TerminalRead::Bytes(count) => {
                            read += count;
                            match mode {
                                TerminalReadMode::Canonical => break,
                                TerminalReadMode::Noncanonical {
                                    minimum,
                                    timeout_ns,
                                } => {
                                    if read == capacity || read >= minimum {
                                        break;
                                    }
                                    if timeout_ns != 0 {
                                        deadline = Some(
                                            crate::timer::get_time_ns().saturating_add(timeout_ns),
                                        );
                                    }
                                }
                            }
                        }
                        TerminalRead::Eof => break,
                    }
                }
                let mut cursor = UserIoCursor::new(vectors);
                let result = cursor.copy_to_user(task, &input[..read]);
                scatter_result(&cursor, result)
            }
        },
    }
}
