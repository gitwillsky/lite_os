use super::*;

/// @description 执行 scalar/writev 共用的唯一 sequential write descriptor dispatch。
/// @param task userspace address owner 与 SIGPIPE/RLIMIT source。
/// @param ofd 已完成 access/capability 检查的共享 OFD。
/// @param vectors scalar one-element 或已导入的 RV64 iovec 序列。
/// @param total_length vectors 的 checked 总长度。
/// @return byte count、partial count 或负 errno。
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
            let inode = opened.inode();
            if inode.inode_type() == InodeType::Directory {
                return -errno::EISDIR;
            }
            let file = match RegularFile::from_inode(inode) {
                Ok(file) => file,
                Err(error) => return ferr(error),
            };
            let append = *ofd.flags.lock() & O_APPEND != 0;
            let mut offset = ofd.offset.lock();
            let writer = match file.begin_write() {
                Ok(writer) => writer,
                Err(error) => return ferr(error),
            };
            write_regular_vectors(task, &writer, &mut offset, vectors, append)
        }
        OpenFileKind::Pipe(endpoint) => {
            if endpoint.direction() != PipeDirection::Write {
                return -errno::EBADF;
            }
            let mut cursor = UserIoCursor::new(vectors);
            let mut input = [0u8; PIPE_BUF];
            let mut written = 0usize;
            while written < total_length {
                // 1. 每次只 gather 一笔 PIPE_BUF 范围内的原子 payload。
                let count = (total_length - written).min(input.len());
                let copied = match cursor.copy_from_user(task, &mut input[..count]) {
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
                    match endpoint.write(&input[..count]) {
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
                            send_thread_signal(task.tgid(), task.tid(), 13)
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
            let mut input = match buffer(capacity) {
                Ok(input) => input,
                Err(error) => return error,
            };
            let mut cursor = UserIoCursor::new(vectors);
            let mut written = 0usize;
            while written < total_length {
                // 2. stream 复用 bounded buffer，并在首次短写/阻塞后返回标准 partial count。
                let requested = (total_length - written).min(input.len());
                match cursor.copy_from_user(task, &mut input[..requested]) {
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
                    match socket.write(&input[..requested]) {
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
                            send_thread_signal(task.tgid(), task.tid(), 13)
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
        OpenFileKind::Epoll(_) => unreachable!("epoll write rejected before descriptor dispatch"),
        OpenFileKind::Character(device) => {
            if let CharacterDevice::Terminal {
                terminal,
                kind: DeviceKind::Tty | DeviceKind::PtySlave(_),
                ..
            } = device
                && let Err(error) = guard_terminal_access(terminal, TerminalAccess::Output)
            {
                return error;
            }
            if matches!(
                device,
                CharacterDevice::Entropy
                    | CharacterDevice::Kmsg(_)
                    | CharacterDevice::Drm(_)
                    | CharacterDevice::Input { .. }
            ) {
                return -errno::EOPNOTSUPP;
            }
            let mut cursor = UserIoCursor::new(vectors);
            let mut input = [0u8; 512];
            let mut written = 0usize;
            while written < total_length {
                let requested = (total_length - written).min(input.len());
                let copied = match cursor.copy_from_user(task, &mut input[..requested]) {
                    Ok(copied) => copied,
                    Err(()) => {
                        return if written == 0 {
                            -errno::EFAULT
                        } else {
                            written as isize
                        };
                    }
                };
                assert_eq!(copied, requested, "character gather ended early");
                let count = match device {
                    CharacterDevice::Null | CharacterDevice::Zero => requested,
                    CharacterDevice::Terminal {
                        pty: Some(slave), ..
                    } => loop {
                        match slave.write(&input[..requested]) {
                            Ok(0) if written != 0 => return written as isize,
                            Ok(0) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                                return -errno::EAGAIN;
                            }
                            Ok(0) => match crate::task::wait_for_pipe(
                                &slave.output_pipe(),
                                PipeWaitCondition::Writable {
                                    minimum: crate::fs::PtySlave::output_write_minimum(requested),
                                },
                            ) {
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
                            },
                            Ok(count) => break count,
                            Err(error) => {
                                return if written == 0 {
                                    ferr(error)
                                } else {
                                    written as isize
                                };
                            }
                        }
                    },
                    CharacterDevice::Terminal {
                        terminal,
                        pty: None,
                        ..
                    } => match terminal.write(&input[..requested]) {
                        Ok(count) => count,
                        Err(error) => {
                            return if written == 0 {
                                ferr(error)
                            } else {
                                written as isize
                            };
                        }
                    },
                    CharacterDevice::PtyMaster(master) => loop {
                        match master.write(&input[..requested]) {
                            Ok(0) if written != 0 => return written as isize,
                            Ok(0) if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                                return -errno::EAGAIN;
                            }
                            Ok(0) => {
                                let wait = match master.prepare_write_to_block() {
                                    None => WaitResult::Woken,
                                    Some(pipe) => crate::task::wait_for_pipe(
                                        &pipe,
                                        PipeWaitCondition::Readable,
                                    ),
                                };
                                match wait {
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
                            Ok(count) => break count,
                            Err(error) => {
                                return if written == 0 {
                                    ferr(error)
                                } else {
                                    written as isize
                                };
                            }
                        }
                    },
                    CharacterDevice::Entropy => unreachable!("entropy write rejected above"),
                    CharacterDevice::Kmsg(_) => unreachable!("kmsg write rejected above"),
                    CharacterDevice::Drm(_) => unreachable!("DRM write rejected above"),
                    CharacterDevice::Input { .. } => unreachable!("input write rejected above"),
                };
                written += count;
                if count < requested {
                    return written as isize;
                }
            }
            written as isize
        }
    }
}
