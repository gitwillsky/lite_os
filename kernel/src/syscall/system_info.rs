use crate::{
    syscall::errno,
    task::{SystemInfoSnapshot, current_task, system_info_snapshot},
};

const SYSINFO_BYTES: usize = 112;
const LOAD_SCALE: u64 = 1 << 16;

/// @description 按 Linux v7.1 RV64 `struct sysinfo` ABI 返回系统运行状态。
///
/// @param address 用户态 112-byte `struct sysinfo` 输出地址。
/// @return 成功返回 0；用户地址不可写返回 `-EFAULT`。
pub(crate) fn sys_sysinfo(address: usize) -> isize {
    let snapshot = system_info_snapshot();
    let bytes = encode_system_info(&snapshot);
    let task = current_task().expect("sysinfo requires a current task");
    if task.copy_to_user(address, &bytes).is_err() {
        -errno::EFAULT
    } else {
        0
    }
}

fn encode_system_info(snapshot: &SystemInfoSnapshot) -> [u8; SYSINFO_BYTES] {
    let mut bytes = [0u8; SYSINFO_BYTES];
    // 1. Linux 将有非零余数的 boot time 向上取整到秒。
    let uptime_seconds = snapshot.uptime_us.saturating_add(999_999) / 1_000_000;
    write_u64(&mut bytes, 0, uptime_seconds);
    // 2. 内部千分制 EWMA 只在 ABI 边界转换为 SI_LOAD_SHIFT=16。
    for (index, load_milli) in snapshot.load_milli.into_iter().enumerate() {
        write_u64(
            &mut bytes,
            8 + index * 8,
            load_milli.saturating_mul(LOAD_SCALE) / 1_000,
        );
    }
    // 3. RV64 可直接用 byte-valued RAM 字段，因此 mem_unit 固定为 1；未实现的
    // swap/highmem/shared/buffer 字段保持零，避免伪造不存在的内核状态。
    write_u64(&mut bytes, 32, snapshot.total_memory_bytes);
    write_u64(&mut bytes, 40, snapshot.free_memory_bytes);
    write_u16(
        &mut bytes,
        80,
        snapshot.task_count.min(u16::MAX as usize) as u16,
    );
    write_u32(&mut bytes, 104, 1);
    bytes
}

fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_ne_bytes());
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}
