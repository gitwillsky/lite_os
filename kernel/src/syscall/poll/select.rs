use alloc::vec::Vec;

use crate::{syscall::errno, task::TaskControlBlock};

use super::{POLLERR, POLLHUP, POLLIN, POLLOUT, POLLPRI};
use crate::syscall::timer::{TimeSpec, decode_timespec};

/// @description pselect6 fd-set input/output codec；wait orchestration 不接触 bitmap storage。
pub(super) struct SelectSets {
    byte_count: usize,
    addresses: [usize; 3],
    input: [Vec<u8>; 3],
}

impl SelectSets {
    /// @description 一次性导入 read/write/except fd sets。
    /// @param task 当前 userspace address-space owner。
    /// @param count fd upper bound。
    /// @param addresses read/write/except userspace addresses；零表示缺省集合。
    /// @return 完整 codec state。
    /// @errors allocation/copy failure 返回 Linux 负 errno。
    pub(super) fn load(
        task: &TaskControlBlock,
        count: usize,
        addresses: [usize; 3],
    ) -> Result<Self, isize> {
        let byte_count = count.div_ceil(8);
        Ok(Self {
            byte_count,
            addresses,
            input: [
                copy_fd_set(task, addresses[0], byte_count)?,
                copy_fd_set(task, addresses[1], byte_count)?,
                copy_fd_set(task, addresses[2], byte_count)?,
            ],
        })
    }

    /// @description 将一个 fd 的三个输入 bitmap 投影为 poll event mask。
    /// @param fd 非负且小于 pselect count 的 descriptor number。
    /// @return POLLIN/POLLOUT/POLLPRI 组合。
    pub(super) fn events(&self, fd: usize) -> i16 {
        let mut events = 0;
        if fd_is_set(&self.input[0], fd) {
            events |= POLLIN;
        }
        if fd_is_set(&self.input[1], fd) {
            events |= POLLOUT;
        }
        if fd_is_set(&self.input[2], fd) {
            events |= POLLPRI;
        }
        events
    }

    /// @description 编码并原样覆盖 pselect 的三个输出集合。
    /// @param task 当前 userspace address-space owner。
    /// @param descriptors `(fd, requested, returned)` readiness 投影。
    /// @return 至少在一个集合中 ready 的 fd 数，或 Linux 负 errno。
    pub(super) fn copy_results(
        &self,
        task: &TaskControlBlock,
        descriptors: impl Iterator<Item = (usize, i16, i16)>,
    ) -> isize {
        let Some(total) = self.byte_count.checked_mul(3) else {
            return -errno::ENOMEM;
        };
        let Ok(mut output) = zeroed_bytes(total) else {
            return -errno::ENOMEM;
        };
        let mut ready = 0;
        for (fd, requested, returned) in descriptors {
            let mut descriptor_ready = false;
            for (set, interested, observed) in [
                (0, POLLIN, POLLIN | POLLERR | POLLHUP),
                (1, POLLOUT, POLLOUT | POLLERR),
                (2, POLLPRI, POLLPRI),
            ] {
                if requested & interested != 0 && returned & observed != 0 {
                    output[set * self.byte_count + fd / 8] |= 1 << (fd % 8);
                    descriptor_ready = true;
                }
            }
            ready += usize::from(descriptor_ready);
        }
        for (set, address) in self.addresses.into_iter().enumerate() {
            let start = set * self.byte_count;
            if address != 0
                && task
                    .copy_to_user(address, &output[start..start + self.byte_count])
                    .is_err()
            {
                return -errno::EFAULT;
            }
        }
        ready as isize
    }
}

/// @description 将 pselect relative timespec 归一化为 monotonic deadline。
/// @param task 当前 userspace address-space owner。
/// @param timeout 可空 RV64 timespec pointer。
/// @return None 表示无限等待，Some 表示 absolute deadline。
/// @errors invalid timespec、overflow 或 copy failure 返回 Linux 负 errno。
pub(super) fn deadline(task: &TaskControlBlock, timeout: usize) -> Result<Option<u64>, isize> {
    if timeout == 0 {
        return Ok(None);
    }
    let mut bytes = [0u8; core::mem::size_of::<TimeSpec>()];
    task.copy_from_user(timeout, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    let value = decode_timespec(&bytes);
    if value.tv_sec < 0 || !(0..1_000_000_000).contains(&value.tv_nsec) {
        return Err(-errno::EINVAL);
    }
    let relative = value
        .tv_sec
        .checked_mul(1_000_000_000)
        .and_then(|seconds| seconds.checked_add(value.tv_nsec))
        .and_then(|value| u64::try_from(value).ok())
        .ok_or(-errno::EINVAL)?;
    crate::timer::get_time_ns()
        .checked_add(relative)
        .map(Some)
        .ok_or(-errno::EINVAL)
}

/// @description 导入 pselect6 `{sigmask,size}` 并发布临时 signal mask。
/// @param task 当前 Thread owner。
/// @param argument 可空 RV64 pair pointer。
/// @return true 表示 caller 必须在 ready/timeout 路径恢复临时 mask。
/// @errors size/copy failure 返回 Linux 负 errno。
pub(super) fn install_signal_mask(task: &TaskControlBlock, argument: usize) -> Result<bool, isize> {
    if argument == 0 {
        return Ok(false);
    }
    let mut pair = [0u8; 16];
    task.copy_from_user(argument, &mut pair)
        .map_err(|_| -errno::EFAULT)?;
    let mask = usize::from_ne_bytes(pair[..8].try_into().unwrap());
    let size = usize::from_ne_bytes(pair[8..].try_into().unwrap());
    if size != 8 {
        return Err(-errno::EINVAL);
    }
    if mask == 0 {
        return Ok(false);
    }
    let mut bytes = [0u8; 8];
    task.copy_from_user(mask, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    task.begin_signal_suspend(u64::from_ne_bytes(bytes));
    Ok(true)
}

fn copy_fd_set(
    task: &TaskControlBlock,
    address: usize,
    byte_count: usize,
) -> Result<Vec<u8>, isize> {
    if address == 0 {
        return Ok(Vec::new());
    }
    let mut bytes = zeroed_bytes(byte_count)?;
    task.copy_from_user(address, &mut bytes)
        .map_err(|_| -errno::EFAULT)?;
    Ok(bytes)
}

fn fd_is_set(bits: &[u8], fd: usize) -> bool {
    bits.get(fd / 8)
        .is_some_and(|byte| byte & (1 << (fd % 8)) != 0)
}

fn zeroed_bytes(length: usize) -> Result<Vec<u8>, isize> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| -errno::ENOMEM)?;
    bytes.resize(length, 0);
    Ok(bytes)
}
