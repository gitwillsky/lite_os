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
    if total_length == 0 {
        return 0;
    }
    let file = match ofd.regular_file() {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let append = ofd.status_flags() & O_APPEND != 0;
    let staging = PreparedRegularWriteStaging::prepare(total_length);
    with_prepared_staging(staging, |staging| {
        let mut staging = staging.as_input_staging();
        ofd.with_position(|offset| {
            let writer = match file.begin_write(task.access_identity(true), append) {
                Ok(writer) => writer,
                Err(error) => return ferr(error),
            };
            write_regular_vectors(task, &writer, offset, vectors, append, &mut staging)
        })
        .unwrap_or_else(crate::syscall::fs::position_error)
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
    nowait: bool,
) -> isize {
    // 语义见 `read_descriptor`：io_uring 的 per-IO NOWAIT，不修改共享 OFD 的 O_NONBLOCK。
    let nonblocking = nowait || ofd.status_flags() & O_NONBLOCK != 0;
    match &ofd.kind {
        OpenFileKind::Inode => {
            write_regular_descriptor(task, ofd, ofd.inode_ref(), vectors, total_length)
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
                        PipeWrite::Full if nonblocking => {
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
        OpenFileKind::Socket(socket) => crate::syscall::socket::write_vectors(
            task,
            ofd,
            socket,
            vectors,
            total_length,
            nonblocking,
            0,
        ),
        OpenFileKind::EventFd(event) => {
            if total_length == 0 {
                return -errno::EINVAL;
            }
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
                        crate::ipc::EventFdWrite::Full if nonblocking => {
                            return if written == 0 {
                                -errno::EAGAIN
                            } else {
                                written as isize
                            };
                        }
                        crate::ipc::EventFdWrite::Full => {
                            match crate::syscall::poll::wait_for_ofd(ofd, 4, None) {
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
        OpenFileKind::TimerFd(_) | OpenFileKind::SignalFd(_) => -errno::EINVAL,
        OpenFileKind::Epoll(_) => unreachable!("epoll write rejected before descriptor dispatch"),
        OpenFileKind::Device(file) => crate::syscall::device::write_device(
            task,
            ofd,
            file.as_ref(),
            vectors,
            total_length,
            nonblocking,
        ),
    }
}
