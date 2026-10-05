use riscv::register;

/// 读取未经验证的 `mhartid`，仅供入口与 panic 诊断。
///
/// # Returns
///
/// 硬件提供的原始 hart ID。
#[inline(always)]
pub(crate) fn raw_hart_id() -> usize {
    register::mhartid::read()
}

/// 获取能由 SBI 单字 hart mask 表达的当前 hart ID。
///
/// # Returns
///
/// 小于 `usize::BITS` 的 hart ID。
///
/// # Errors
///
/// 越界表示入口不变量破坏并触发 panic，绝不映射到其他 hart。
#[inline(always)]
pub(crate) fn hart_id() -> usize {
    let hart = raw_hart_id();
    assert!(
        hart < crate::constants::HART_MASK_BITS,
        "mhartid {} exceeds SBI hart-mask width {}",
        hart,
        crate::constants::HART_MASK_BITS
    );
    hart
}
