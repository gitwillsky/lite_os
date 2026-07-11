/// 最大支持的核心数量。
pub const MAX_CORES: usize = 8;

/// 获取当前硬件线程 ID。
#[inline(always)]
pub fn hart_id() -> usize {
    let tp_value: usize;
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) tp_value);
    }

    if tp_value >= MAX_CORES { 0 } else { tp_value }
}
