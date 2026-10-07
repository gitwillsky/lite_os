use super::*;
use crate::fs::Inode;
use core::mem::MaybeUninit;

fn write_regular_descriptor(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    inode: Arc<dyn Inode>,
    vectors: &[UserIoVec],
    total_length: usize,
) -> isize {
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    let file = match RegularFile::from_inode(inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let append = *ofd.flags.lock() & O_APPEND != 0;
    let staging = PreparedRegularWriteStaging::prepare(total_length);
    with_prepared_staging(staging, |staging| {
        let mut staging = staging.as_input_staging();
        ofd.with_position(|offset| {
            let writer = match file.begin_write() {
                Ok(writer) => writer,
                Err(error) => return ferr(error),
            };
            write_regular_vectors(task, &writer, offset, vectors, append, &mut staging)
        })
    })
}

/// 执行 scalar/writev 共用的唯一 sequential write descriptor dispatch。
///
/// # Parameters
///
/// - `task`: userspace address owner 与 SIGPIPE/RLIMIT source。
/// - `ofd`: 已完成 access/capability 检查的共享 OFD。
/// - `vectors`: scalar one-element 或已导入的 LP64 iovec 序列。
/// - `total_length`: vectors 的 checked 总长度。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(super) fn write_descriptor(
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
            let result = write_regular_descriptor(task, ofd, opened.inode(), vectors, total_length);
            if result > 0 {
                crate::fs::notify_opened(opened, crate::fs::IN_MODIFY);
            }
            result
        }
        OpenFileKind::MemFile(file) => {
            write_regular_descriptor(task, ofd, file.clone(), vectors, total_length)
        }
        OpenFileKind::Pipe(endpoint) => {
            if endpoint.direction() != PipeDirection::Write {
                return -errno::EBADF;
            }
            let mut cursor = UserIoCursor::new(vectors);
            let mut storage = [MaybeUninit::uninit(); PIPE_BUF];
            let mut input = UserInputStaging::from_slice(&mut storage);
            let mut written = 0usize;
            while written < total_length {
                // 1. 每次只 gather 一笔 PIPE_BUF 范围内的原子 payload。
                let count = (total_length - written).min(input.capacity());
                let copied = match cursor.copy_from_user_into(task, &mut input, count) {
                    Ok(copied) => copied,
                    Err(()) => {
                        return if written == 0 {
                            -errno::EFAULT
                        } else {
                            written as isize
                        };
                    }
                };
                assert_eq!(
                    copied, count,
                    "sequential pipe gather ended before its checked total"
                );
                loop {
                    // 2. 仅在整笔 payload 可提交时推进 pipe-visible progress。
                    match endpoint.write(input.initialized()) {
                        PipeWrite::Bytes(count) => {
                            written += count;
                            break;
                        }
                        PipeWrite::Full if written != 0 => return written as isize,
                        PipeWrite::Full if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                            return -errno::EAGAIN;
                        }
                        PipeWrite::Full => {
                            if let Err(error) = block_on_pipe(
                                &endpoint.pipe(),
                                PipeWaitCondition::Writable { minimum: count },
                            ) {
                                return error;
                            }
                        }
                        PipeWrite::Broken => {
                            // 3. peer close 始终投递 SIGPIPE；已有进度时 syscall 只暴露 partial count。
                            send_thread_signal(
                                task.tgid(),
                                task.tid(),
                                syscall_abi::signal::SIGPIPE,
                            )
                            .expect("current sequential pipe writer must exist");
                            return if written == 0 {
                                -errno::EPIPE
                            } else {
                                written as isize
                            };
                        }
                    }
                }
            }
            written as isize
        }
        OpenFileKind::Socket(socket) => {
            if let Err(error) = socket.validate_send_length(total_length) {
                return crate::syscall::socket::socket_error(error);
            }
            // 1. stream 使用 facade 选择的 bounded staging；atomic protocol 仍一次 gather
            // 完整消息，避免拆成多个数据报。
            let capacity = socket
                .stream_send_staging_capacity(total_length, 64 * 1024)
                .unwrap_or(total_length);
            let mut input = match UserInputStaging::try_new(capacity) {
                Ok(input) => input,
                Err(()) => return -errno::ENOMEM,
            };
            let mut cursor = UserIoCursor::new(vectors);
            let mut written = 0usize;
            while written < total_length {
                // 2. stream 复用 bounded buffer，并在首次短写/阻塞后返回标准 partial count。
                let requested = (total_length - written).min(input.capacity());
                match cursor.copy_from_user_into(task, &mut input, requested) {
                    Ok(copied) => {
                        assert_eq!(copied, requested, "socket gather ended early")
                    }
                    Err(()) => {
                        return if written == 0 {
                            -errno::EFAULT
                        } else {
                            written as isize
                        };
                    }
                }
                loop {
                    match socket.write(input.initialized()) {
                        Ok(count) => {
                            written += count;
                            if count < requested {
                                return written as isize;
                            }
                            break;
                        }
                        Err(crate::socket::SocketSendError::WouldBlock) if written != 0 => {
                            return written as isize;
                        }
                        Err(
                            crate::socket::SocketSendError::WouldBlock
                            | crate::socket::SocketSendError::PeerFull(_),
                        ) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                            return -errno::EAGAIN;
                        }
                        Err(crate::socket::SocketSendError::WouldBlock) => {
                            match crate::syscall::poll::wait_for_ofd(ofd, 4) {
                                WaitResult::Woken => {}
                                WaitResult::Interrupted => return -errno::EINTR,
                                WaitResult::TimedOut => unreachable!(),
                                WaitResult::OutOfMemory => return -errno::ENOMEM,
                            }
                        }
                        Err(crate::socket::SocketSendError::PeerFull(blocker)) => {
                            match crate::syscall::poll::wait_for_socket_send(&blocker) {
                                WaitResult::Woken => {}
                                WaitResult::Interrupted => return -errno::EINTR,
                                WaitResult::TimedOut => unreachable!(),
                                WaitResult::OutOfMemory => return -errno::ENOMEM,
                            }
                        }
                        Err(crate::socket::SocketSendError::Error(
                            crate::socket::SocketError::BrokenPipe,
                        )) => {
                            // 3. 即使已有进度，peer close 仍投递 SIGPIPE，但返回值保留已写 byte count。
                            send_thread_signal(
                                task.tgid(),
                                task.tid(),
                                syscall_abi::signal::SIGPIPE,
                            )
                            .expect("current sequential socket writer must exist");
                            return if written == 0 {
                                -errno::EPIPE
                            } else {
                                written as isize
                            };
                        }
                        Err(crate::socket::SocketSendError::Error(error)) => {
                            return if written == 0 {
                                crate::syscall::socket::socket_error(error)
                            } else {
                                written as isize
                            };
                        }
                    }
                }
            }
            written as isize
        }
        OpenFileKind::EventFd(event) => {
            let mut written = 0usize;
            // Linux eventfd 只实现 scalar write；统一 engine 中 scalar 是一个 vector，
            // writev fallback 则逐个非空 iovec 调用 write，不能合并跨 iovec 的八字节前缀。
            for vector in vectors {
                if vector.length == 0 {
                    continue;
                }
                if vector.length != mem::size_of::<u64>() {
                    return if written == 0 {
                        -errno::EINVAL
                    } else {
                        written as isize
                    };
                }
                let mut bytes = [0u8; mem::size_of::<u64>()];
                if task.copy_from_user(vector.base, &mut bytes).is_err() {
                    return if written == 0 {
                        -errno::EFAULT
                    } else {
                        written as isize
                    };
                }
                let value = u64::from_ne_bytes(bytes);
                if value == u64::MAX {
                    return if written == 0 {
                        -errno::EINVAL
                    } else {
                        written as isize
                    };
                }
                loop {
                    match event.write(value) {
                        crate::ipc::EventFdWrite::Written => {
                            written += mem::size_of::<u64>();
                            break;
                        }
                        crate::ipc::EventFdWrite::Full if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                            return if written == 0 {
                                -errno::EAGAIN
                            } else {
                                written as isize
                            };
                        }
                        crate::ipc::EventFdWrite::Full => {
                            match crate::syscall::poll::wait_for_ofd(ofd, 4) {
                                WaitResult::Woken => {}
                                WaitResult::Interrupted => {
                                    return if written == 0 {
                                        -errno::EINTR
                                    } else {
                                        written as isize
                                    };
                                }
                                WaitResult::TimedOut => unreachable!(),
                                WaitResult::OutOfMemory => {
                                    return if written == 0 {
                                        -errno::ENOMEM
                                    } else {
                                        written as isize
                                    };
                                }
                            }
                        }
                    }
                }
            }
            written as isize
        }
        OpenFileKind::TimerFd(_) => -errno::EINVAL,
        OpenFileKind::Epoll(_) => unreachable!("epoll write rejected before descriptor dispatch"),
        OpenFileKind::Device(file) => {
            crate::syscall::device::write_device(task, ofd, file.as_ref(), vectors, total_length)
        }
    }
}
