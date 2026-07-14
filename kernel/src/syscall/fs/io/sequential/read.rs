use super::*;

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
            // 1. stream 使用 bounded staging；datagram/raw 保留 caller 的完整 receive capacity。
            let capacity = match socket.socket_type() {
                crate::socket::SocketType::Stream => total_length.min(64 * 1024),
                crate::socket::SocketType::Datagram | crate::socket::SocketType::Raw => {
                    total_length
                }
            };
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
            CharacterDevice::Entropy(_) => {
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
            CharacterDevice::Terminal {
                terminal: console,
                kind,
            } => {
                let mut input = [0u8; 512];
                let read = loop {
                    if *kind == DeviceKind::Tty
                        && let Err(error) = guard_terminal_access(console, TerminalAccess::Input)
                    {
                        return error;
                    }
                    if drain_terminal_input(console).is_err() {
                        return -errno::EIO;
                    }
                    let count = total_length.min(input.len());
                    match console.read(&mut input[..count]) {
                        TerminalRead::Empty => {
                            match crate::task::wait_for_console(|| console.wait_ready()) {
                                crate::task::WaitResult::Woken => continue,
                                crate::task::WaitResult::Interrupted => return -errno::EINTR,
                                crate::task::WaitResult::TimedOut => {
                                    panic!("console wait cannot time out")
                                }
                            }
                        }
                        TerminalRead::Bytes(read) => break read,
                        TerminalRead::Eof => return 0,
                    }
                };
                let mut cursor = UserIoCursor::new(vectors);
                let result = cursor.copy_to_user(task, &input[..read]);
                scatter_result(&cursor, result)
            }
        },
    }
}
