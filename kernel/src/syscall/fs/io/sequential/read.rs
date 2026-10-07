use super::*;
use crate::fs::Inode;
use crate::ipc::ReceiveBuffer;

fn read_regular_descriptor(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    inode: Arc<dyn Inode>,
    vectors: &[UserIoVec],
) -> isize {
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    let file = match RegularFile::from_inode(inode) {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    // 单个 sequential read 唯一持有 OFD offset；缺失该 ownership 会让共享 OFD
    // 的并发 reader 在 chunks 之间穿插，使一次 operation 返回不连续的文件区间。
    ofd.with_position(|offset| read_regular_vectors(task, &file, offset, vectors))
}

/// 执行 scalar/readv 共用的唯一 sequential read descriptor dispatch。
///
/// # Parameters
///
/// - `task`: userspace address owner。
/// - `ofd`: 已完成 access/capability 检查的共享 OFD。
/// - `vectors`: scalar one-element 或已导入的 LP64 iovec 序列。
/// - `total_length`: vectors 的 checked 总 capacity。
///
/// # Returns
///
/// byte count、EOF、partial count 或负 errno。
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
            let result = read_regular_descriptor(task, ofd, opened.inode(), vectors);
            if result > 0 {
                crate::fs::notify_opened(opened, crate::fs::IN_ACCESS);
            }
            result
        }
        OpenFileKind::MemFile(file) => read_regular_descriptor(task, ofd, file.clone(), vectors),
        OpenFileKind::Pipe(endpoint) => {
            if endpoint.direction() != PipeDirection::Read {
                return -errno::EBADF;
            }
            let mut input = match ReceiveBuffer::try_new(total_length.min(64 * 1024)) {
                Ok(input) => input,
                Err(()) => return -errno::ENOMEM,
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
            assert_eq!(read, input.len());
            let result = cursor.copy_to_user(task, input.initialized());
            scatter_result(&cursor, result)
        }
        OpenFileKind::Socket(socket) => {
            // 1. Socket facade 从唯一 protocol policy 投影 bounded useful capacity。
            let capacity = socket.receive_staging_capacity(total_length, 64 * 1024);
            let mut input = match ReceiveBuffer::try_new(capacity) {
                Ok(input) => input,
                Err(()) => return -errno::ENOMEM,
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
            assert_eq!(read, input.len());
            let result = cursor.copy_to_user(task, input.initialized());
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
        OpenFileKind::TimerFd(timer) => {
            let size = mem::size_of::<u64>();
            if total_length < size {
                return -errno::EINVAL;
            }
            let mut cursor = UserIoCursor::new(vectors);
            if cursor.validate_write_prefix(task, size).is_err() {
                return -errno::EFAULT;
            }
            let expirations = loop {
                match timer.read() {
                    crate::file::TimerFdRead::Expirations(value) => break value,
                    crate::file::TimerFdRead::Empty if *ofd.flags.lock() & O_NONBLOCK != 0 => {
                        return -errno::EAGAIN;
                    }
                    crate::file::TimerFdRead::Empty => {
                        match crate::syscall::poll::wait_for_ofd(ofd, 1) {
                            WaitResult::Woken => {}
                            WaitResult::Interrupted => return -errno::EINTR,
                            WaitResult::TimedOut => unreachable!(),
                            WaitResult::OutOfMemory => return -errno::ENOMEM,
                        }
                    }
                }
            };
            if cursor
                .copy_to_user(task, &expirations.to_ne_bytes())
                .is_err()
            {
                return -errno::EFAULT;
            }
            size as isize
        }
        OpenFileKind::Epoll(_) => unreachable!("epoll read rejected before descriptor dispatch"),
        OpenFileKind::Device(file) => {
            crate::syscall::device::read_device(task, ofd, file.as_ref(), vectors, total_length)
        }
    }
}
