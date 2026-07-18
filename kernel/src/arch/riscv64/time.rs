/// @description 读取 RISC-V supervisor 可见的 monotonic time counter。
///
/// @return 当前 64-bit `time` CSR 值。
#[inline(always)]
pub(crate) fn counter() -> u64 {
    riscv::register::time::read64()
}
