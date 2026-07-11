use core::sync::atomic;

use alloc::vec::Vec;

use crate::{
    arch::sbi,
    memory::page_table::translated_byte_buffer,
    syscall::errno::{EBADF, EINVAL, EIO, ENOSYS, ESRCH},
    task::{current_task, current_user_token, suspend_current_and_run_next},
};

const STDIN_FILENO: usize = 0;
const STDOUT_FILENO: usize = 1;

/// @description 向文件描述符写入用户缓冲区中的字节。
///
/// @param fd 目标文件描述符。
/// @param buf 用户缓冲区起始地址。
/// @param len 请求写入的字节数。
/// @return 写入字节数，失败返回负 errno。
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    if fd == STDOUT_FILENO {
        let buffers = translated_byte_buffer(current_user_token(), buf, len);
        let mut written = 0usize;
        for buffer in buffers {
            for byte in buffer.iter().copied() {
                if sbi::console_putchar(byte).is_err() {
                    return if written == 0 { -EIO } else { written as isize };
                }
                written += 1;
            }
        }
        return written as isize;
    }

    let Some(task) = current_task() else {
        return -ESRCH;
    };
    let Some(descriptor) = task.file.lock().fd(fd) else {
        return -EBADF;
    };

    let buffers = translated_byte_buffer(current_user_token(), buf, len);
    let mut data = Vec::with_capacity(len);
    for buffer in buffers {
        data.extend_from_slice(buffer);
    }
    descriptor
        .write_at(&data)
        .map(|written| written as isize)
        .unwrap_or(-EINVAL)
}

/// @description 从文件描述符读取字节到用户缓冲区。
///
/// @param fd 源文件描述符。
/// @param buf 用户缓冲区起始地址。
/// @param len 最多读取的字节数。
/// @return 读取字节数，失败返回负 errno。
pub fn sys_read(fd: usize, buf: *mut u8, len: usize) -> isize {
    if len == 0 {
        return 0;
    }

    if fd == STDIN_FILENO {
        let byte = loop {
            match sbi::console_getchar() {
                Ok(Some(byte)) => break byte,
                Ok(None) => suspend_current_and_run_next(),
                Err(_) => return -EIO,
            }
        };
        let mut buffers = translated_byte_buffer(current_user_token(), buf.cast_const(), 1);
        let Some(destination) = buffers.first_mut().and_then(|buffer| buffer.first_mut()) else {
            return -EINVAL;
        };
        *destination = byte;
        return 1;
    }

    let Some(task) = current_task() else {
        return -ESRCH;
    };
    let Some(descriptor) = task.file.lock().fd(fd) else {
        return -EBADF;
    };

    let mut data = alloc::vec![0u8; len];
    let read = match descriptor.read_at(&mut data) {
        Ok(read) => read,
        Err(_) => return -EINVAL,
    };
    let buffers = translated_byte_buffer(current_user_token(), buf.cast_const(), read);
    let mut copied = 0usize;
    for buffer in buffers {
        let count = buffer.len().min(read - copied);
        buffer[..count].copy_from_slice(&data[copied..copied + count]);
        copied += count;
    }
    read as isize
}

/// @description 关闭文件描述符。
///
/// @param fd 待关闭的文件描述符。
/// @return 成功返回零，失败返回负 errno。
pub fn sys_close(fd: usize) -> isize {
    if matches!(fd, STDIN_FILENO | STDOUT_FILENO) {
        return 0;
    }
    let Some(task) = current_task() else {
        return -ESRCH;
    };
    if task.file.lock().close_fd(fd) {
        0
    } else {
        -EBADF
    }
}

/// @description 修改打开文件描述符的偏移。
///
/// @param fd 目标文件描述符。
/// @param offset 相对偏移。
/// @param whence `SEEK_SET/SEEK_CUR/SEEK_END`。
/// @return 新偏移，失败返回负 errno。
pub fn sys_lseek(fd: usize, offset: isize, whence: usize) -> isize {
    let Some(task) = current_task() else {
        return -ESRCH;
    };
    let Some(descriptor) = task.file.lock().fd(fd) else {
        return -EBADF;
    };

    let base = match whence {
        0 => 0i128,
        1 => descriptor.offset.load(atomic::Ordering::Relaxed) as i128,
        2 => descriptor.inode.size() as i128,
        _ => return -EINVAL,
    };
    let new_offset = base + offset as i128;
    if !(0..=u64::MAX as i128).contains(&new_offset) {
        return -EINVAL;
    }
    descriptor
        .offset
        .store(new_offset as u64, atomic::Ordering::Release);
    new_offset as isize
}

/// @description 将当前工作目录复制到用户缓冲区。
///
/// @param buf 用户缓冲区起始地址。
/// @param len 缓冲区长度。
/// @return 成功返回包含 NUL 的字节数，失败返回负 errno。
pub fn sys_get_cwd(buf: *mut u8, len: usize) -> isize {
    let Some(task) = current_task() else {
        return -ESRCH;
    };
    let cwd = task.cwd.lock();
    let required = cwd.len() + 1;
    if len < required {
        return -EINVAL;
    }

    let mut bytes = Vec::with_capacity(required);
    bytes.extend_from_slice(cwd.as_bytes());
    bytes.push(0);
    let buffers = translated_byte_buffer(current_user_token(), buf.cast_const(), required);
    let mut copied = 0usize;
    for buffer in buffers {
        let count = buffer.len().min(required - copied);
        buffer[..count].copy_from_slice(&bytes[copied..copied + count]);
        copied += count;
    }
    required as isize
}

/// @description 复制文件描述符到最低可用编号。
///
/// @param fd 源文件描述符。
/// @return 新文件描述符，失败返回负 errno。
pub fn sys_dup(fd: usize) -> isize {
    let Some(task) = current_task() else {
        return -ESRCH;
    };
    task.file
        .lock()
        .dup_fd(fd)
        .map(|new_fd| new_fd as isize)
        .unwrap_or(-EBADF)
}

/// @description 执行当前支持的 `fcntl` 命令子集。
///
/// @param fd 目标文件描述符。
/// @param cmd Linux `fcntl` 命令。
/// @param arg 命令参数。
/// @return 命令结果，未支持命令返回 `EINVAL`。
pub fn sys_fcntl(fd: usize, cmd: i32, _arg: usize) -> isize {
    const F_GETFL: i32 = 3;

    let Some(task) = current_task() else {
        return -ESRCH;
    };
    if fd == STDIN_FILENO {
        return if cmd == F_GETFL { 0 } else { -ENOSYS };
    }

    let Some(descriptor) = task.file.lock().fd(fd) else {
        return -EBADF;
    };
    match cmd {
        F_GETFL => descriptor.flags as isize,
        _ => -ENOSYS,
    }
}
