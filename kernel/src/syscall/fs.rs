use alloc::vec::Vec;

use crate::{
    arch::sbi,
    syscall::errno::{EBADF, EFAULT, EIO, ERANGE, ESRCH},
    task::current_task,
};

const STDOUT_FILENO: usize = 1;

/// @description 向文件描述符写入用户缓冲区中的字节。
///
/// @param fd 目标文件描述符。
/// @param buf 用户缓冲区起始地址。
/// @param len 请求写入的字节数。
/// @return 写入字节数，失败返回负 errno。
pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    if fd != STDOUT_FILENO {
        return -EBADF;
    }
    if len == 0 {
        return 0;
    }
    let Some(task) = current_task() else {
        return -ESRCH;
    };

    let mut chunk = [0u8; 256];
    let mut written = 0usize;
    while written < len {
        let count = chunk.len().min(len - written);
        let Some(address) = (buf as usize).checked_add(written) else {
            return if written == 0 {
                -EFAULT
            } else {
                written as isize
            };
        };
        if task.copy_from_user(address, &mut chunk[..count]).is_err() {
            return if written == 0 {
                -EFAULT
            } else {
                written as isize
            };
        }
        for byte in chunk[..count].iter().copied() {
            if sbi::console_putchar(byte).is_err() {
                return if written == 0 { -EIO } else { written as isize };
            }
            written += 1;
        }
    }
    written as isize
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
    let cwd = task.cwd();
    let required = cwd.len() + 1;
    if len < required {
        return -ERANGE;
    }

    let mut bytes = Vec::with_capacity(required);
    bytes.extend_from_slice(cwd.as_bytes());
    bytes.push(0);
    if task.copy_to_user(buf as usize, &bytes).is_err() {
        return -EFAULT;
    }
    required as isize
}
