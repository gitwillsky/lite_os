use riscv::register;

/// 获取当前硬件线程ID
#[inline(always)]
pub fn hart_id() -> usize {
    register::mhartid::read()
}

/// 最大支持的核心数量
pub const MAX_CORES: usize = 8;

/// 检查核心ID是否有效
#[inline]
pub fn is_valid_hart_id(hart_id: usize) -> bool {
    hart_id < MAX_CORES
}