use super::*;

/// Linux limits one readv/writev transaction to 1024 iovec entries.
pub(super) const IOV_MAX: usize = 1024;

/// Linux RV64 userspace `struct iovec` layout.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct UserIoVec {
    pub(super) base: usize,
    pub(super) length: usize,
}

/// @description 按 Linux RV64 `struct iovec` 顺序从同一个 OFD scatter read。
///
/// @param fd 源 descriptor。
/// @param iovector userspace `iovec` 数组地址；count 为零时可为空。
/// @param count iovec 数量，最大 1024。
/// @return 总读取字节数；导入失败或首个 read 失败返回负 errno，已有进度后返回 partial count。
pub(crate) fn sys_readv(fd: usize, iovector: usize, count: usize) -> isize {
    if count > IOV_MAX {
        return -errno::EINVAL;
    }
    if count == 0 {
        return sys_read(fd, core::ptr::null_mut(), 0);
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
    let stream = matches!(&ofd.kind, OpenFileKind::Pipe(_))
        || matches!(&ofd.kind, OpenFileKind::Character(device) if device.terminal().is_some());
    let mut vectors = Vec::new();
    if vectors.try_reserve_exact(count).is_err() {
        return -errno::ENOMEM;
    }
    let mut total_length = 0usize;
    for index in 0..count {
        let Some(address) = index
            .checked_mul(mem::size_of::<UserIoVec>())
            .and_then(|offset| iovector.checked_add(offset))
        else {
            return -errno::EFAULT;
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
    if let OpenFileKind::Pipe(endpoint) = &ofd.kind {
        if endpoint.direction() != PipeDirection::Read {
            return -errno::EBADF;
        }
        let mut input = Vec::new();
        let capacity = total_length.min(64 * 1024);
        if input.try_reserve_exact(capacity).is_err() {
            return -errno::ENOMEM;
        }
        input.resize(capacity, 0);
        let read = loop {
            match endpoint.read(&mut input) {
                PipeRead::Bytes(read) => break read,
                PipeRead::Eof => return 0,
                PipeRead::Empty if *ofd.flags.lock() & O_NONBLOCK != 0 => return -errno::EAGAIN,
                PipeRead::Empty => {
                    if let Err(error) = block_on_pipe(&endpoint.pipe(), PipeWaitCondition::Readable)
                    {
                        return error;
                    }
                }
            }
        };
        let mut copied = 0;
        for vector in vectors {
            let count = vector.length.min(read - copied);
            if count == 0 {
                break;
            }
            if task
                .copy_to_user(vector.base, &input[copied..copied + count])
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
    let mut read = 0usize;
    for vector in vectors {
        if vector.length == 0 {
            continue;
        }
        let result = sys_read(fd, vector.base as *mut u8, vector.length);
        if result < 0 {
            return if read == 0 { result } else { read as isize };
        }
        let result = result as usize;
        read += result;
        if result < vector.length || stream && result != 0 {
            break;
        }
    }
    read as isize
}
