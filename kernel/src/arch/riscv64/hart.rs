/// 最大支持的核心数量
pub const MAX_CORES: usize = 8;

/// 获取当前硬件线程ID（S模式适配版本）
/// 使用TP寄存器存储当前核心ID，增加边界检查
#[inline(always)]
pub fn hart_id() -> usize {
    let tp_value: usize;
    unsafe {
        core::arch::asm!("mv {}, tp", out(reg) tp_value);
    }
    
    // 简单的边界检查，类似Linux的做法
    if tp_value >= MAX_CORES {
        // 返回0作为安全的默认值，避免数组越界
        0
    } else {
        tp_value
    }
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