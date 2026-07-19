use crate::{random::EntropyBatch, syscall::errno, task::current_task};

use super::getrandom_flags::getrandom_flags_supported;

/// @description 以 virtio-rng 为唯一 entropy source 实现 Linux getrandom。
pub(crate) fn sys_getrandom(buffer: usize, length: usize, flags: usize) -> isize {
    if !getrandom_flags_supported(flags) {
        return -errno::EINVAL;
    }
    if length == 0 {
        return 0;
    }
    if buffer == 0 {
        return -errno::EFAULT;
    }
    let task = current_task().expect("getrandom requires a current task");
    let mut written = 0usize;
    let mut chunk = match EntropyBatch::<4096>::try_new() {
        Some(chunk) => chunk,
        None => return -errno::ENOMEM,
    };
    while written < length {
        let count = 4096.min(length - written);
        let bytes = match chunk.fill(count) {
            Ok(bytes) => bytes,
            Err(_) => {
                return if written == 0 {
                    -errno::EIO
                } else {
                    written as isize
                };
            }
        };
        let Some(address) = buffer.checked_add(written) else {
            return if written == 0 {
                -errno::EFAULT
            } else {
                written as isize
            };
        };
        if task.copy_to_user(address, bytes).is_err() {
            return if written == 0 {
                -errno::EFAULT
            } else {
                written as isize
            };
        }
        written += count;
    }
    written as isize
}
