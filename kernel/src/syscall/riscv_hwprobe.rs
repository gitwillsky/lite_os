use crate::{system, task::current_task};

use super::errno;

const PAIR_SIZE: usize = 16;

/// @description 实现 Linux/riscv64 `riscv_hwprobe` value-query ABI，并保守公布所有 hart 共同能力。
/// @param pairs userspace `struct riscv_hwprobe` 数组地址。
/// @param pair_count 数组元素数量。
/// @param cpusetsize 可选 hart mask 的 byte 数；零且 cpus 为 null 表示全部 online hart。
/// @param cpus 可选 little-endian hart mask 地址。
/// @param flags 当前只接受 value-query 的零 flags。
/// @return 成功返回零；无 online hart、flags、地址或长度无效时返回 Linux 负 errno。
pub(crate) fn sys_riscv_hwprobe(
    pairs: usize,
    pair_count: usize,
    cpusetsize: usize,
    cpus: usize,
    flags: usize,
) -> isize {
    if flags != 0 {
        return -errno::EINVAL;
    }
    if pair_count != 0 && pairs == 0 {
        return -errno::EFAULT;
    }
    if pair_count.checked_mul(PAIR_SIZE).is_none() {
        return -errno::EFAULT;
    }
    if let Err(error) = validate_hart_mask(cpusetsize, cpus) {
        return error;
    }

    let task = current_task().expect("riscv_hwprobe requires current task");
    for index in 0..pair_count {
        let Some(address) = index
            .checked_mul(PAIR_SIZE)
            .and_then(|offset| pairs.checked_add(offset))
        else {
            return -errno::EFAULT;
        };
        let mut pair = [0u8; PAIR_SIZE];
        if task.copy_from_user(address, &mut pair[..8]).is_err() {
            return -errno::EFAULT;
        }
        let key = i64::from_ne_bytes(pair[..8].try_into().unwrap());
        let (key, value) = probe_value(key);
        pair[..8].copy_from_slice(&key.to_ne_bytes());
        pair[8..].copy_from_slice(&value.to_ne_bytes());
        if task.copy_to_user(address, &pair).is_err() {
            return -errno::EFAULT;
        }
    }
    0
}

fn validate_hart_mask(cpusetsize: usize, cpus: usize) -> Result<(), isize> {
    if cpusetsize == 0 && cpus == 0 {
        return Ok(());
    }
    if cpus == 0 {
        return Err(-errno::EFAULT);
    }
    let count = cpusetsize.min(size_of::<usize>());
    let mut bytes = [0u8; size_of::<usize>()];
    current_task()
        .unwrap()
        .copy_from_user(cpus, &mut bytes[..count])
        .map_err(|_| -errno::EFAULT)?;
    let requested = usize::from_ne_bytes(bytes);
    if requested & system::online_hart_mask() == 0 {
        return Err(-errno::EINVAL);
    }
    Ok(())
}

fn probe_value(key: i64) -> (i64, u64) {
    system::riscv_hwprobe_value(key).map_or((-1, 0), |value| (key, value))
}
