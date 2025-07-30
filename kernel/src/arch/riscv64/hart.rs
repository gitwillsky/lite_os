/// 最大支持的核心数量
pub const MAX_CORES: usize = 8;

/// 获取当前硬件线程ID（S模式适配版本）
/// 使用TP寄存器存储当前核心ID
#[inline(always)]
pub fn hart_id() -> usize {
    let tp_value: usize;
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) tp_value);
    }
    tp_value
}

/// 设置当前核心ID到TP寄存器
/// 在每个核心启动时调用
#[inline]
pub fn set_hart_id(id: usize) {
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) id);
    }
}

/// 检查核心ID是否有效
#[inline]
pub fn is_valid_hart_id(hart_id: usize) -> bool {
    hart_id < MAX_CORES
}